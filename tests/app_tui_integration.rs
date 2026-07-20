mod support;

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

#[cfg(feature = "agent-observability")]
use latte_lens::agent::*;
#[cfg(feature = "navigation-test-support")]
use latte_lens::navigation::{AppOptions, NavigationSettings};
use latte_lens::{
    app::{App, ContentMode, FocusPane, GitRowKind, SearchMode, TreeScope},
    preview::{HighlightKind, PreviewContent, PreviewProvider, PreviewRegistry, PreviewRequest},
    ui,
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind},
    style::{Color, Modifier},
};
use support::TestRepo;
#[cfg(feature = "agent-observability")]
use support::agent::{FakeAdapter, digest};
use unicode_width::UnicodeWidthStr;

#[test]
fn startup_renders_before_the_initial_tree_snapshot_is_ready() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("plain.txt"), "hello\n").unwrap();

    let mut app = App::new(directory.path().to_path_buf()).unwrap();
    assert!(app.is_initial_loading());
    assert!(app.is_refreshing());
    assert!(app.all_entries.is_empty());

    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("loading workspace"));
    assert!(rendered.contains("Scanning files"));
    assert!(rendered.contains("Loading workspace"));

    settle(&mut app);
    assert!(!app.is_initial_loading());
    assert!(
        app.all_entries
            .iter()
            .any(|entry| entry.relative == Path::new("plain.txt"))
    );
}

#[test]
fn preview_folding_renders_markers_and_find_reveals_hidden_body() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join("fold.rs"),
        "fn folded() {\n    let hidden_needle = 1;\n    println!(\"{}\", hidden_needle);\n}\n",
    )
    .unwrap();
    let mut app = ready_app(directory.path().to_path_buf()).unwrap();
    assert_eq!(app.content_mode, ContentMode::Preview);
    app.handle_key(key(KeyCode::Char('l')));
    app.handle_key(modified_key(KeyCode::Char('{'), KeyModifiers::SHIFT));

    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains('▸'));
    assert!(rendered.contains("lines"));
    assert!(!rendered.contains("hidden_needle"));

    let marker_index = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .position(|cell| cell.symbol() == "▸")
        .unwrap();
    let marker_column = u16::try_from(marker_index % 100).unwrap();
    let marker_row = u16::try_from(marker_index / 100).unwrap();
    app.handle_mouse(mouse_down(marker_column, marker_row.saturating_add(5)));
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(
        rendered.contains('▸'),
        "blank EOF rows must not alias the marker"
    );

    app.handle_mouse(mouse_down(marker_column, marker_row));
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("hidden_needle"));

    app.handle_key(modified_key(KeyCode::Char('{'), KeyModifiers::SHIFT));

    app.handle_key(modified_key(KeyCode::Char('f'), KeyModifiers::CONTROL));
    for character in "hidden_needle".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("hidden_needle"));
}

#[test]
fn huge_raw_content_scroll_renders_and_navigates_from_the_effective_end() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("plain.txt"), "one\ntwo\nthree\n").unwrap();
    let mut app = ready_app(directory.path().to_path_buf()).unwrap();
    app.handle_key(key(KeyCode::Char('l')));
    app.content_scroll = usize::MAX;

    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("three"));

    app.handle_key(key(KeyCode::Up));
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("two"));
    assert_ne!(app.content_scroll, usize::MAX);
}

#[test]
fn provisional_repository_scan_does_not_flash_partial_in_git_changes() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir_all(directory.path().join("one/two/three")).unwrap();

    let mut app = ready_app(directory.path().to_path_buf()).unwrap();
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(!rendered.contains("PARTIAL"));
    assert!(!rendered.contains("[partial]"));

    app.set_tree_scope(TreeScope::GitChanges);
    assert!(app.is_refreshing());
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(!rendered.contains("PARTIAL"));
    assert!(!rendered.contains("[partial]"));

    settle(&mut app);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(!rendered.contains("PARTIAL"));
    assert!(!rendered.contains("[partial]"));
}

#[test]
fn every_workspace_starts_with_two_levels_and_loads_deeper_directories_on_expand() {
    let parent = tempfile::tempdir().unwrap();
    let workspace = parent.path().join("arbitrary-workspace");
    fs::create_dir_all(workspace.join("one/two/child")).unwrap();
    fs::write(workspace.join("one/two/deep.txt"), "deep\n").unwrap();
    fs::write(workspace.join("one/two/child/nested.txt"), "nested\n").unwrap();

    let mut app = ready_app(workspace).unwrap();
    let initial_paths: Vec<_> = app
        .all_entries
        .iter()
        .map(|entry| entry.relative.clone())
        .collect();
    assert!(initial_paths.contains(&PathBuf::from("one/two")));
    assert!(!initial_paths.contains(&PathBuf::from("one/two/deep.txt")));
    assert_eq!(visible_paths(&app), [PathBuf::from("one")]);

    app.handle_key(key(KeyCode::Enter));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));

    assert!(app.is_directory_loading());
    assert!(
        app.content_lines
            .iter()
            .any(|line| line.contains("Loading"))
    );
    settle(&mut app);
    assert!(!app.is_directory_loading());
    assert!(visible_paths(&app).contains(&PathBuf::from("one/two/deep.txt")));
    assert!(visible_paths(&app).contains(&PathBuf::from("one/two/child")));
    assert!(!visible_paths(&app).contains(&PathBuf::from("one/two/child/nested.txt")));
}

#[test]
fn app_starts_from_nested_path_and_renders_both_panes() {
    let fixture = TestRepo::new();
    fixture.write("src/lib.rs", "pub fn original() {}\n");
    fixture.commit_all("initial");
    fixture.write("src/lib.rs", "pub fn changed() {}\n");
    fixture.write("notes.txt", "untracked\n");

    let mut app = ready_app(fixture.root().join("src")).unwrap();
    assert_eq!(
        app.root.canonicalize().unwrap(),
        fixture.root().join("src").canonicalize().unwrap()
    );
    assert_eq!(app.branch.as_deref(), Some("main"));
    assert_eq!(app.changed_count, 1);
    assert_eq!(app.tree_scope, TreeScope::AllFiles);
    assert_eq!(app.content_mode, ContentMode::Preview);

    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("LATTE LENS"));
    assert!(rendered.contains("Files"));
    assert!(rendered.contains("1 Files"));
    assert!(rendered.contains("2 Git changes"));
    assert!(rendered.contains("Refresh"));
    assert!(rendered.contains("Preview"));
    assert!(!rendered.contains("1 TREE"));
    assert!(app.ui_regions.tree_body.width < 100);
    assert!(app.ui_regions.content_body.x > app.ui_regions.tree_body.x);

    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("lib.rs")));
    assert!(
        app.content_lines
            .iter()
            .any(|line| line == "+pub fn changed() {}")
    );
}

#[test]
fn quit_keys_confirm_while_ctrl_c_quits_without_a_selection() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("plain.txt"), "hello\n").unwrap();
    let mut app = ready_app(directory.path().to_path_buf()).unwrap();
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    app.handle_key(key(KeyCode::Char('q')));
    assert!(!app.should_quit());
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    assert!(format!("{:?}", terminal.backend().buffer()).contains("Press q again"));

    app.handle_key(key(KeyCode::Char('h')));
    app.handle_key(key(KeyCode::Char('q')));
    assert!(
        !app.should_quit(),
        "a non-quit key must disarm confirmation"
    );
    app.handle_key(key(KeyCode::Char('q')));
    assert!(app.should_quit());

    let mut escape_app = ready_app(directory.path().to_path_buf()).unwrap();
    escape_app.handle_key(key(KeyCode::Esc));
    assert!(!escape_app.should_quit());
    escape_app.handle_key(key(KeyCode::Esc));
    assert!(escape_app.should_quit());

    let mut ctrl_c_app = ready_app(directory.path().to_path_buf()).unwrap();
    ctrl_c_app.handle_key(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(ctrl_c_app.should_quit());
}

#[test]
fn partial_scope_is_marked_without_painting_a_background() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("plain.txt"), "hello\n").unwrap();
    let mut app = ready_app(directory.path().to_path_buf()).unwrap();
    app.all_files_truncated = true;

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("1+ · PARTIAL"));
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .all(|cell| cell.bg == Color::Reset)
    );

    app.set_tree_scope(TreeScope::GitChanges);
    app.git_changes_truncated = true;
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("0+ · PARTIAL"));
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .all(|cell| cell.bg == Color::Reset)
    );
}

#[test]
fn narrow_repository_layout_clips_labels_and_hitboxes_without_backgrounds() {
    let fixture = TestRepo::new();
    fixture.write(
        "a-very-long-directory-name/a-very-long-file-name.txt",
        "before\n",
    );
    fixture.commit_all("initial");
    fixture.write(
        "a-very-long-directory-name/a-very-long-file-name.txt",
        "after\n",
    );
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);

    let backend = TestBackend::new(32, 10);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    let tree_end = app
        .ui_regions
        .tree_body
        .x
        .saturating_add(app.ui_regions.tree_body.width);
    assert!(
        app.ui_regions
            .git_changes_tab
            .x
            .saturating_add(app.ui_regions.git_changes_tab.width)
            <= tree_end
    );
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .all(|cell| cell.bg == Color::Reset)
    );
}

#[test]
fn keyboard_switches_tree_scope_without_hiding_right_content() {
    let fixture = TestRepo::new();
    fixture.write("a-clean.txt", "clean\n");
    fixture.write("b-changed.txt", "before\n");
    fixture.commit_all("initial");
    fixture.write("b-changed.txt", "after\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("a-clean.txt"))
    );
    assert_eq!(app.content_mode, ContentMode::Preview);
    app.handle_key(key(KeyCode::Char('l')));
    assert_eq!(app.focused_pane, FocusPane::Content);

    app.handle_key(key(KeyCode::Tab));
    settle(&mut app);
    assert_eq!(app.tree_scope, TreeScope::GitChanges);
    assert_eq!(app.focused_pane, FocusPane::Content);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("b-changed.txt"))
    );
    assert_eq!(app.content_mode, ContentMode::Diff);
    assert!(app.content_lines.iter().any(|line| line == "+after"));

    app.handle_key(key(KeyCode::BackTab));
    settle(&mut app);
    assert_eq!(app.tree_scope, TreeScope::AllFiles);
    assert_eq!(app.focused_pane, FocusPane::Content);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("b-changed.txt"))
    );
    assert_eq!(app.content_mode, ContentMode::Preview);

    app.handle_key(key(KeyCode::Char('2')));
    settle(&mut app);
    app.handle_key(key(KeyCode::Char('p')));
    settle(&mut app);
    assert_eq!(app.tree_scope, TreeScope::GitChanges);
    assert_eq!(app.focused_pane, FocusPane::Content);
    assert_eq!(app.content_mode, ContentMode::Preview);
    app.handle_key(key(KeyCode::Char('d')));
    settle(&mut app);
    assert_eq!(app.content_mode, ContentMode::Diff);
    assert_eq!(app.focused_pane, FocusPane::Content);
}

#[test]
fn tree_top_up_focuses_scope_tabs_and_down_restores_tree_selection() {
    let fixture = TestRepo::new();
    fixture.write("a.txt", "a\n");
    fixture.write("b.txt", "b\n");
    fixture.commit_all("initial");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("a.txt")));
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.focused_pane, FocusPane::ScopeTabs);
    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("a.txt")));

    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.focused_pane, FocusPane::Tree);
    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("a.txt")));

    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.focused_pane, FocusPane::Tree);
    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("a.txt")));

    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.focused_pane, FocusPane::ScopeTabs);
    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("a.txt")));
}

#[test]
fn scope_tab_arrows_switch_scope_through_the_refresh_path() {
    let fixture = TestRepo::new();
    fixture.write("a-clean.txt", "clean\n");
    fixture.write("b-changed.txt", "before\n");
    fixture.commit_all("initial");
    fixture.write("b-changed.txt", "after\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.focused_pane, FocusPane::ScopeTabs);
    app.handle_key(key(KeyCode::Left));
    #[cfg(feature = "agent-observability")]
    assert_eq!(app.tree_scope, TreeScope::Agents);
    #[cfg(not(feature = "agent-observability"))]
    assert_eq!(app.tree_scope, TreeScope::GitChanges);
    app.handle_key(key(KeyCode::Right));
    settle(&mut app);
    assert_eq!(app.tree_scope, TreeScope::AllFiles);
    assert_eq!(app.focused_pane, FocusPane::ScopeTabs);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("a-clean.txt"))
    );

    // This change happens after startup, so Right must enter Git Changes via
    // the same refresh path as the existing scope controls.
    fixture.write("z-untracked.txt", "new\n");
    app.handle_key(key(KeyCode::Right));
    settle(&mut app);
    assert_eq!(app.tree_scope, TreeScope::GitChanges);
    assert_eq!(app.focused_pane, FocusPane::ScopeTabs);
    assert_eq!(app.changed_count, 2);
    let changed_paths: Vec<_> = app
        .visible_entries()
        .iter()
        .map(|entry| entry.relative.clone())
        .collect();
    assert_eq!(
        changed_paths,
        [
            PathBuf::from("b-changed.txt"),
            PathBuf::from("z-untracked.txt")
        ]
    );
    assert!(!changed_paths.contains(&PathBuf::from("a-clean.txt")));

    app.handle_key(key(KeyCode::Left));
    assert_eq!(app.tree_scope, TreeScope::AllFiles);
    assert_eq!(app.focused_pane, FocusPane::ScopeTabs);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("b-changed.txt"))
    );
}

#[test]
fn visible_rows_keep_raw_datasets_canonical_and_apply_scope_defaults() {
    let fixture = TestRepo::new();
    fixture.write("docs/clean.md", "clean\n");
    fixture.write("src/nested/changed.rs", "before\n");
    fixture.commit_all("initial");
    fixture.write("src/nested/changed.rs", "after\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    // The scan remains complete, but the first All Files projection only
    // exposes roots because directories default collapsed.
    let raw_all_paths: Vec<_> = app
        .all_entries
        .iter()
        .map(|entry| entry.relative.clone())
        .collect();
    assert!(raw_all_paths.contains(&PathBuf::from("docs/clean.md")));
    assert!(raw_all_paths.contains(&PathBuf::from("src/nested")));
    assert!(!raw_all_paths.contains(&PathBuf::from("src/nested/changed.rs")));
    assert!(
        app.all_entries
            .iter()
            .find(|entry| entry.relative == Path::new("src/nested"))
            .is_some_and(|entry| entry.contains_changes)
    );
    assert_eq!(
        visible_paths(&app),
        [PathBuf::from("docs"), PathBuf::from("src")]
    );

    // Opening one parent only reveals its direct rows; nested directories are
    // still collapsed until explicitly opened.
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        visible_paths(&app),
        [
            PathBuf::from("docs"),
            PathBuf::from("docs/clean.md"),
            PathBuf::from("src"),
        ]
    );

    app.set_tree_scope(TreeScope::GitChanges);
    // Git Changes starts with every required changed ancestor expanded.
    let changed_paths = visible_paths(&app);
    assert_eq!(
        changed_paths,
        [
            PathBuf::from("src"),
            PathBuf::from("src/nested"),
            PathBuf::from("src/nested/changed.rs"),
        ]
    );
    assert_eq!(app.content_mode, ContentMode::Diff);
}

#[test]
fn enter_toggles_only_directories_and_keeps_files_on_the_content_pane() {
    let fixture = TestRepo::new();
    fixture.write("src/file.txt", "fixture\n");
    fixture.write("top.txt", "top\n");
    fixture.commit_all("initial");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    assert_eq!(
        visible_paths(&app),
        [PathBuf::from("src"), PathBuf::from("top.txt")]
    );
    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("src")));

    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.focused_pane, FocusPane::Tree);
    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("src")));
    assert_eq!(
        visible_paths(&app),
        [
            PathBuf::from("src"),
            PathBuf::from("src/file.txt"),
            PathBuf::from("top.txt"),
        ]
    );
    assert_eq!(
        app.content_lines,
        [
            "No changed files in this directory.",
            "",
            "Expanded · Enter or click to collapse."
        ]
    );

    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.focused_pane, FocusPane::Tree);
    assert_eq!(
        visible_paths(&app),
        [PathBuf::from("src"), PathBuf::from("top.txt")]
    );

    app.handle_key(key(KeyCode::Enter));
    app.handle_key(key(KeyCode::Down));
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("src/file.txt"))
    );
    let rows_before_file_enter = visible_paths(&app);

    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.focused_pane, FocusPane::Content);
    assert_eq!(visible_paths(&app), rows_before_file_enter);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("src/file.txt"))
    );
}

#[test]
fn mouse_click_toggles_directories_once_and_leaves_file_expansion_alone() {
    let fixture = TestRepo::new();
    fixture.write("src/file.txt", "fixture\n");
    fixture.write("top.txt", "top\n");
    fixture.commit_all("initial");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    let tree_x = app.ui_regions.tree_inner.x;
    let tree_y = app.ui_regions.tree_inner.y;
    app.handle_mouse(mouse_down(tree_x, tree_y));
    assert_eq!(app.focused_pane, FocusPane::Tree);
    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("src")));
    assert_eq!(
        app.content_lines,
        [
            "No changed files in this directory.",
            "",
            "Expanded · Enter or click to collapse."
        ]
    );
    assert_eq!(
        visible_paths(&app),
        [
            PathBuf::from("src"),
            PathBuf::from("src/file.txt"),
            PathBuf::from("top.txt"),
        ]
    );

    // A single second click on the same directory closes it again; no double
    // click path is required to toggle state.
    app.handle_mouse(mouse_down(tree_x, tree_y));
    assert_eq!(
        visible_paths(&app),
        [PathBuf::from("src"), PathBuf::from("top.txt")]
    );

    app.handle_mouse(mouse_down(tree_x, tree_y));
    let rows_before_file_click = visible_paths(&app);
    app.handle_mouse(mouse_down(tree_x, tree_y + 1));

    assert_eq!(app.focused_pane, FocusPane::Tree);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("src/file.txt"))
    );
    assert_eq!(app.content_mode, ContentMode::Preview);
    assert_eq!(visible_paths(&app), rows_before_file_click);
}

#[test]
fn refresh_preserves_scope_choices_and_defaults_new_directories() {
    let fixture = TestRepo::new();
    fixture.write("alpha/old.txt", "before\n");
    fixture.commit_all("initial");
    fixture.write("alpha/old.txt", "after\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    // All Files keeps an explicit expansion through refresh, while a new
    // directory remains collapsed.
    app.handle_key(key(KeyCode::Enter));
    fixture.write("beta/new.txt", "new\n");
    app.handle_key(key(KeyCode::Char('r')));
    settle(&mut app);
    let all_after_refresh = visible_paths(&app);
    assert!(all_after_refresh.contains(&PathBuf::from("alpha/old.txt")));
    assert!(all_after_refresh.contains(&PathBuf::from("beta")));
    assert!(!all_after_refresh.contains(&PathBuf::from("beta/new.txt")));

    // Git Changes starts expanded, retains a deliberate collapse, and opens a
    // newly discovered changed directory by default.
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    app.handle_key(key(KeyCode::Home));
    assert!(app.selected_git_row().is_some_and(|row| row.is_container()));
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("alpha")));
    app.handle_key(key(KeyCode::Enter));
    fixture.write("gamma/new.txt", "new\n");
    app.handle_key(key(KeyCode::Char('r')));
    settle(&mut app);
    let changed_after_refresh = visible_paths(&app);
    assert!(changed_after_refresh.contains(&PathBuf::from("alpha")));
    assert!(!changed_after_refresh.contains(&PathBuf::from("alpha/old.txt")));
    assert!(changed_after_refresh.contains(&PathBuf::from("beta/new.txt")));
    assert!(changed_after_refresh.contains(&PathBuf::from("gamma/new.txt")));
}

#[test]
fn hidden_saved_selection_falls_back_to_its_visible_ancestor_after_a_refresh() {
    let fixture = TestRepo::new();
    fixture.write("tracked.txt", "tracked\n");
    fixture.commit_all("initial");
    fixture.write("src/child.txt", "untracked\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    app.handle_key(key(KeyCode::Enter));
    app.handle_key(key(KeyCode::Down));
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("src/child.txt"))
    );

    // Keep All Files' child selection saved while refreshes in the other scope
    // remove and later rediscover the parent. Rediscovery uses the new All
    // Files default (collapsed), so restoring the child must select `src`.
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    fs::remove_dir_all(fixture.root().join("src")).unwrap();
    app.handle_key(key(KeyCode::Char('r')));
    settle(&mut app);
    fixture.write("src/child.txt", "returned\n");
    app.handle_key(key(KeyCode::Char('r')));
    settle(&mut app);
    app.set_tree_scope(TreeScope::AllFiles);

    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("src")));
    assert!(app.tree_state.offset() < app.visible_entries().len());
    assert!(!visible_paths(&app).contains(&PathBuf::from("src/child.txt")));
}

#[test]
fn scope_switch_tracks_changed_files_and_preserves_the_git_fallback() {
    let fixture = TestRepo::new();
    fixture.write("a-changed.txt", "before a\n");
    fixture.write("b-clean.txt", "clean\n");
    fixture.write("c-changed.txt", "before c\n");
    fixture.commit_all("initial");
    fixture.write("a-changed.txt", "after a\n");
    fixture.write("c-changed.txt", "after c\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("a-changed.txt"))
    );
    app.handle_key(key(KeyCode::Down));
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("c-changed.txt"))
    );

    // A selected changed file follows the scope switch back to Files.
    app.set_tree_scope(TreeScope::AllFiles);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("c-changed.txt"))
    );

    // A clean Files selection must leave Git Changes on its prior choice.
    app.handle_key(key(KeyCode::Up));
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("b-clean.txt"))
    );
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("c-changed.txt"))
    );

    // The refreshed status is authoritative. If the Files candidate looked
    // changed in the old snapshot but is now clean, retain the prior Git row.
    app.set_tree_scope(TreeScope::AllFiles);
    app.handle_key(key(KeyCode::Up));
    app.handle_key(key(KeyCode::Up));
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("a-changed.txt"))
    );
    fixture.write("a-changed.txt", "before a\n");
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("c-changed.txt"))
    );

    // If that saved Git choice becomes clean too, keep the established
    // changed-file-first fallback instead of following the clean Files row.
    app.set_tree_scope(TreeScope::AllFiles);
    fixture.write("a-changed.txt", "after a\n");
    fixture.write("c-changed.txt", "before c\n");
    app.handle_key(key(KeyCode::Up));
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("b-clean.txt"))
    );
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("a-changed.txt"))
    );
}

#[test]
fn scope_switch_reveals_a_nested_changed_file_in_both_trees() {
    let fixture = TestRepo::new();
    fixture.write("src/changed.txt", "before\n");
    fixture.commit_all("initial");
    fixture.write("src/changed.txt", "after\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    // Files starts with `src` collapsed. Git Changes selects the nested file;
    // returning to Files must expand the path and select the file itself.
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("src/changed.txt"))
    );
    app.set_tree_scope(TreeScope::AllFiles);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("src/changed.txt"))
    );
    assert!(visible_paths(&app).contains(&PathBuf::from("src/changed.txt")));

    // Preserve a deliberate Git-tree collapse until the Files selection asks
    // to reveal that changed file again.
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("src")));
    app.handle_key(key(KeyCode::Enter));
    assert!(!visible_paths(&app).contains(&PathBuf::from("src/changed.txt")));
    app.set_tree_scope(TreeScope::AllFiles);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("src/changed.txt"))
    );
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("src/changed.txt"))
    );
    assert!(visible_paths(&app).contains(&PathBuf::from("src/changed.txt")));
}

#[test]
fn scope_switch_keeps_same_named_changes_in_nested_repositories_isolated() {
    let fixture = TestRepo::new();
    fixture.write("src/shared.txt", "root before\n");
    fixture.commit_all("root initial");
    fixture.write("src/shared.txt", "root after\n");

    let nested = fixture.root().join("nested");
    init_repo(&nested);
    write_file(&nested, "src/shared.txt", "nested before\n");
    git(&nested, &["add", "--all"]);
    git(&nested, &["commit", "--quiet", "-m", "nested initial"]);
    write_file(&nested, "src/shared.txt", "nested after\n");
    let nested_root = nested.canonicalize().unwrap();

    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    let nested_change_index = app
        .visible_git_rows()
        .iter()
        .position(|row| {
            matches!(
                &row.kind,
                GitRowKind::Change(change) if change.path.repo_id.path() == nested_root
            )
        })
        .expect("nested repository change row");
    app.tree_state.select(Some(nested_change_index));

    app.set_tree_scope(TreeScope::AllFiles);
    settle(&mut app);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("nested/src/shared.txt"))
    );

    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    assert!(matches!(
        app.selected_git_row().map(|row| &row.kind),
        Some(GitRowKind::Change(change)) if change.path.repo_id.path() == nested_root
    ));
}

#[test]
fn entering_git_changes_refreshes_and_excludes_clean_files() {
    let fixture = TestRepo::new();
    fixture.write("a-clean.txt", "clean\n");
    fixture.write("b-also-clean.txt", "clean\n");
    fixture.commit_all("initial");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    // Simulate an agent changing the worktree after Latte Lens has started.
    fixture.write("z-new-change.txt", "new\n");
    app.handle_key(key(KeyCode::Char('2')));
    settle(&mut app);

    assert_eq!(app.tree_scope, TreeScope::GitChanges);
    assert_eq!(app.changed_count, 1);
    let changed_paths: Vec<_> = app
        .visible_entries()
        .iter()
        .map(|entry| entry.relative.clone())
        .collect();
    assert_eq!(changed_paths, [PathBuf::from("z-new-change.txt")]);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("z-new-change.txt"))
    );
    assert_eq!(app.content_mode, ContentMode::Diff);
}

#[test]
fn clean_root_repository_is_a_selectable_empty_git_changes_node() {
    let fixture = TestRepo::new();
    fixture.write("tracked.txt", "clean\n");
    fixture.commit_all("initial");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);

    assert_eq!(app.total_repository_count, 1);
    assert_eq!(app.dirty_repository_count, 0);
    assert_eq!(app.changed_count, 0);
    assert_eq!(app.visible_git_rows().len(), 1);
    let row = app.selected_git_row().expect("clean repository selection");
    assert!(matches!(
        row.kind,
        GitRowKind::Repository {
            change_count: 0,
            ..
        }
    ));
    let repository_name = fixture
        .root()
        .file_name()
        .expect("fixture repository name")
        .to_string_lossy();
    assert_eq!(row.label, repository_name);
    assert_ne!(row.label, ".");
    assert!(row.detail.contains("clean"));
    assert!(!row.detail.contains("files"));
    assert_eq!(app.content_mode, ContentMode::Info);
    assert!(app.content_lines[0].contains("0 changed files"));

    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.visible_git_rows().len(), 1);
    assert!(app.selected_git_row().is_some());
    assert!(app.last_error.is_none());
}

#[test]
fn non_git_workspace_lists_clean_and_dirty_descendant_repositories() {
    let workspace = tempfile::tempdir().unwrap();
    let clean = workspace.path().join("clean");
    let dirty = workspace.path().join("dirty");
    for root in [&clean, &dirty] {
        init_repo(root);
        write_file(root, "tracked.txt", "before\n");
        git(root, &["add", "--all"]);
        git(root, &["commit", "--quiet", "-m", "initial"]);
    }
    write_file(&dirty, "tracked.txt", "after\n");

    let mut app = ready_app(workspace.path().to_path_buf()).unwrap();
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);

    assert_eq!(app.total_repository_count, 2);
    assert_eq!(app.dirty_repository_count, 1);
    assert_eq!(app.changed_count, 1);
    let repositories: Vec<_> = app
        .visible_git_rows()
        .iter()
        .filter_map(|row| match row.kind {
            GitRowKind::Repository { change_count, .. } => {
                Some((row.label.as_str(), change_count, row.detail.as_str()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(repositories.len(), 2);
    assert!(repositories.iter().any(|(label, count, detail)| {
        *label == "clean" && *count == 0 && detail.contains("clean")
    }));
    assert!(
        repositories
            .iter()
            .any(|(label, count, _)| *label == "dirty" && *count == 1)
    );
    assert_eq!(
        app.visible_git_rows()
            .iter()
            .filter(|row| matches!(row.kind, GitRowKind::Change(_)))
            .count(),
        1
    );

    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    for (label, should_be_dirty) in [("clean", false), ("dirty", true)] {
        let index = app
            .visible_git_rows()
            .iter()
            .position(|row| row.label == label && matches!(row.kind, GitRowKind::Repository { .. }))
            .unwrap();
        let row_y = app.ui_regions.tree_inner.y + u16::try_from(index).unwrap();
        let marker = (app.ui_regions.tree_inner.x..app.ui_regions.tree_inner.right())
            .filter_map(|x| terminal.backend().buffer().cell((x, row_y)))
            .find(|cell| cell.symbol() == "•");
        if should_be_dirty {
            let marker = marker.expect("dirty repository marker");
            assert_eq!(marker.fg, Color::Rgb(196, 151, 126));
        } else {
            assert!(marker.is_none(), "clean repository must stay quiet");
        }
    }
}

#[test]
fn recursive_clean_submodules_are_separate_empty_repository_nodes() {
    let sources = tempfile::tempdir().unwrap();
    let grand_source = sources.path().join("grand-source");
    init_repo(&grand_source);
    write_file(&grand_source, "grand.txt", "grand\n");
    git(&grand_source, &["add", "--all"]);
    git(&grand_source, &["commit", "--quiet", "-m", "grand initial"]);

    let child_source = sources.path().join("child-source");
    init_repo(&child_source);
    write_file(&child_source, "child.txt", "child\n");
    git(&child_source, &["add", "--all"]);
    git(&child_source, &["commit", "--quiet", "-m", "child initial"]);
    git_with_path(
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
    git(&child_source, &["add", "--all"]);
    git(
        &child_source,
        &["commit", "--quiet", "-m", "add nested submodule"],
    );

    let parent = TestRepo::new();
    git_with_path(
        parent.root(),
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
    parent.commit_all("add child");
    git(
        parent.root(),
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

    let mut app = ready_app(parent.root().to_path_buf()).unwrap();
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);

    assert_eq!(app.total_repository_count, 3);
    assert_eq!(app.dirty_repository_count, 0);
    assert_eq!(app.changed_count, 0);
    let repositories: Vec<_> = app
        .visible_git_rows()
        .iter()
        .filter(|row| matches!(row.kind, GitRowKind::Repository { .. }))
        .map(|row| (row.depth, row.label.as_str(), row.detail.as_str()))
        .collect();
    assert_eq!(
        repositories
            .iter()
            .map(|(_, label, _)| *label)
            .collect::<Vec<_>>(),
        [".", "modules/child", "modules/child/nested/grand"]
    );
    assert_eq!(
        repositories
            .iter()
            .map(|(depth, _, _)| *depth)
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
    assert!(
        repositories
            .iter()
            .all(|(_, _, detail)| detail.contains("clean"))
    );
    assert_eq!(app.visible_git_rows().len(), 3);
}

#[test]
fn clean_deinitialized_submodule_stays_visible_without_inflating_dirty_count() {
    let source = tempfile::tempdir().unwrap();
    init_repo(source.path());
    write_file(source.path(), "tracked.txt", "clean\n");
    git(source.path(), &["add", "--all"]);
    git(source.path(), &["commit", "--quiet", "-m", "initial"]);

    let parent = TestRepo::new();
    git_with_path(
        parent.root(),
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
        ],
        source.path(),
        &["modules/child"],
    );
    parent.commit_all("add child");
    git(
        parent.root(),
        &["submodule", "deinit", "--force", "--", "modules/child"],
    );

    let mut app = ready_app(parent.root().to_path_buf()).unwrap();
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);

    assert_eq!(app.total_repository_count, 2);
    assert_eq!(app.dirty_repository_count, 0);
    assert_eq!(app.changed_count, 0);
    let repositories: Vec<_> = app
        .visible_git_rows()
        .iter()
        .filter_map(|row| match row.kind {
            GitRowKind::Repository {
                kind, change_count, ..
            } => Some((row.label.as_str(), row.detail.as_str(), kind, change_count)),
            _ => None,
        })
        .collect();
    assert_eq!(repositories.len(), 2);
    assert!(repositories.iter().any(|(label, detail, kind, count)| {
        *label == "modules/child"
            && detail.contains("submodule placeholder")
            && detail.contains("uninitialized")
            && detail.contains("clean")
            && *kind == latte_lens::repo_graph::RepoKind::SubmodulePlaceholder
            && *count == 0
    }));
}

#[test]
fn non_git_directory_previews_text_and_has_an_empty_changes_scope() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("plain.txt"), "hello\n").unwrap();
    let mut app = ready_app(directory.path().to_path_buf()).unwrap();

    assert!(app.repo.is_none());
    assert_eq!(app.changed_count, 0);
    assert_eq!(app.content_mode, ContentMode::Preview);
    assert_eq!(app.content_provider.as_deref(), Some("text"));
    assert_eq!(app.content_lines, ["hello"]);

    app.set_tree_scope(TreeScope::GitChanges);
    assert!(app.visible_entries().is_empty());
    assert_eq!(app.content_mode, ContentMode::Info);
    assert_eq!(
        app.content_lines,
        ["Workspace is not a Git repository and has no changed descendant repositories."]
    );
}

#[test]
fn git_changes_groups_root_and_nested_repositories_and_routes_same_names_by_owner() {
    let fixture = TestRepo::new();
    fixture.write("same.txt", "root before\n");
    fixture.commit_all("root initial");
    fixture.write("same.txt", "root after\n");

    let nested = fixture.root().join("vendor/nested");
    init_repo(&nested);
    write_file(&nested, "same.txt", "nested before\n");
    git(&nested, &["add", "--all"]);
    git(&nested, &["commit", "--quiet", "-m", "nested initial"]);
    write_file(&nested, "same.txt", "nested after\n");

    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    assert_eq!(app.changed_count, 2);

    let repository_labels: Vec<_> = app
        .visible_git_rows()
        .iter()
        .filter(|row| matches!(row.kind, latte_lens::app::GitRowKind::Repository { .. }))
        .map(|row| row.label.clone())
        .collect();
    assert_eq!(repository_labels, [".", "vendor/nested"]);
    let changes: Vec<_> = app
        .visible_git_rows()
        .iter()
        .filter_map(|row| match &row.kind {
            latte_lens::app::GitRowKind::Change(change) => Some(change.path.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(changes.len(), 2);
    assert_eq!(changes[0].relative, changes[1].relative);
    assert_ne!(changes[0].repo_id, changes[1].repo_id);
    assert!(
        app.visible_git_rows()
            .iter()
            .any(|row| row.detail.contains("untracked in parent"))
    );

    assert!(app.content_lines.iter().any(|line| line == "+root after"));
    app.handle_key(key(KeyCode::Char('n')));
    settle(&mut app);
    assert!(app.content_lines.iter().any(|line| line == "+nested after"));
    assert!(!app.content_lines.iter().any(|line| line == "+root after"));
    let nested_selection = app
        .selected_git_row()
        .map(|row| row.identity.clone())
        .unwrap();
    fixture.write("another-root-change.txt", "new\n");
    app.handle_key(key(KeyCode::Char('r')));
    settle(&mut app);
    assert_eq!(app.changed_count, 3);
    assert_eq!(
        app.selected_git_row().map(|row| &row.identity),
        Some(&nested_selection)
    );
    assert!(app.content_lines.iter().any(|line| line == "+nested after"));

    let backend = TestBackend::new(100, 22);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .all(|cell| cell.bg == Color::Reset)
    );

    app.handle_key(key(KeyCode::Home));
    assert_eq!(app.visible_git_rows().len(), 5);
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.visible_git_rows().len(), 1);
    assert_eq!(app.changed_count, 3);
    app.handle_mouse(mouse_down(
        app.ui_regions.tree_inner.x,
        app.ui_regions.tree_inner.y,
    ));
    assert_eq!(app.visible_git_rows().len(), 5);
}

#[test]
fn git_changes_sorts_directories_before_files_at_each_level() {
    let fixture = TestRepo::new();
    let paths = [
        "z-root.txt",
        "a-root.txt",
        "z-dir/middle.txt",
        "a-dir/z-child.txt",
        "a-dir/a-child.txt",
        "a-dir/b-dir/inner.txt",
    ];
    for path in paths {
        fixture.write(path, "before\n");
    }
    fixture.commit_all("initial tree order fixture");
    for path in paths {
        fixture.write(path, "after\n");
    }

    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);

    let rows: Vec<_> = app
        .visible_git_rows()
        .iter()
        .map(|row| (row.depth, row.label.as_str()))
        .collect();
    let repository_name = fixture
        .root()
        .file_name()
        .expect("fixture repository name")
        .to_string_lossy();
    assert_eq!(rows[0], (0, repository_name.as_ref()));
    assert_eq!(
        &rows[1..],
        [
            (2, "a-dir"),
            (3, "b-dir"),
            (4, "inner.txt"),
            (3, "a-child.txt"),
            (3, "z-child.txt"),
            (2, "z-dir"),
            (3, "middle.txt"),
            (2, "a-root.txt"),
            (2, "z-root.txt"),
        ]
    );
}

#[test]
fn same_named_repo_directories_keep_summaries_selection_and_diff_ownership_isolated() {
    let fixture = TestRepo::new();
    fixture.write("src/shared.txt", "root before\n");
    fixture.commit_all("root initial");
    fixture.write("src/shared.txt", "root after\n");

    let nested = fixture.root().join("nested");
    init_repo(&nested);
    write_file(&nested, "src/shared.txt", "nested before\n");
    git(&nested, &["add", "--all"]);
    git(&nested, &["commit", "--quiet", "-m", "nested initial"]);
    write_file(&nested, "src/shared.txt", "nested after\n");

    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);

    app.handle_key(key(KeyCode::Home));
    app.handle_key(key(KeyCode::Down));
    let root_directory = app
        .selected_git_row()
        .map(|row| row.identity.clone())
        .unwrap();
    assert_eq!(app.content_lines[0], "1 changed file in this directory.");

    // Collapse and reopen only the root repo's `src`, then select the nested
    // repo's independently identified `src` row.
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.visible_git_rows().len(), 5);
    app.handle_key(key(KeyCode::Enter));
    for _ in 0..3 {
        app.handle_key(key(KeyCode::Down));
    }
    let nested_directory = app
        .selected_git_row()
        .map(|row| row.identity.clone())
        .unwrap();
    assert_ne!(root_directory, nested_directory);
    assert_eq!(app.content_lines[0], "1 changed file in this directory.");

    // Refresh selection and directory info by the complete repo+path identity.
    app.handle_key(key(KeyCode::Char('r')));
    settle(&mut app);
    assert_eq!(
        app.selected_git_row().map(|row| &row.identity),
        Some(&nested_directory)
    );
    assert_eq!(app.content_lines[0], "1 changed file in this directory.");

    // Nested collapse must not hide the root repo's same-named descendant.
    app.handle_key(key(KeyCode::Enter));
    let visible_changes: Vec<_> = app
        .visible_git_rows()
        .iter()
        .filter_map(|row| match &row.kind {
            latte_lens::app::GitRowKind::Change(change) => Some(change.path.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(visible_changes.len(), 1);
    assert!(visible_changes[0].repo_id.path() == fixture.root().canonicalize().unwrap());

    app.handle_key(key(KeyCode::Enter));
    app.handle_key(key(KeyCode::Down));
    settle(&mut app);
    assert!(app.content_lines.iter().any(|line| line == "+nested after"));
    assert!(!app.content_lines.iter().any(|line| line == "+root after"));
}

#[test]
fn nonrepo_workspace_keeps_dirty_descendant_repo_and_discovery_error_visible() {
    let workspace = tempfile::tempdir().unwrap();
    let nested = workspace.path().join("good-repo");
    init_repo(&nested);
    write_file(&nested, "changed.txt", "before\n");
    git(&nested, &["add", "--all"]);
    git(&nested, &["commit", "--quiet", "-m", "initial"]);
    write_file(&nested, "changed.txt", "after\n");
    fs::create_dir_all(workspace.path().join("bad-repo/.git")).unwrap();

    let mut app = ready_app(workspace.path().to_path_buf()).unwrap();
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);

    assert_eq!(app.total_repository_count, 1);
    assert_eq!(app.dirty_repository_count, 1);
    assert_eq!(app.changed_count, 1);
    assert!(app.repository_error_count > 0);
    assert!(
        app.visible_git_rows()
            .iter()
            .any(|row| row.label == "good-repo")
    );
    assert!(
        app.visible_git_rows()
            .iter()
            .any(|row| row.label.contains("[error] bad-repo"))
    );
    assert!(app.content_lines.iter().any(|line| line == "+after"));

    let backend = TestBackend::new(100, 18);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    assert!(format!("{:?}", terminal.backend().buffer()).contains("workspace not repo"));
}

#[test]
fn submodule_pointer_internal_change_and_placeholder_are_separate_rows() {
    let source = TestRepo::new();
    source.write("tracked.txt", "source initial\n");
    source.commit_all("source initial");

    let parent = TestRepo::new();
    let output = Command::new("git")
        .args([
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
        ])
        .arg(source.root())
        .arg("child")
        .current_dir(parent.root())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "submodule add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let modules = parent.root().join(".gitmodules");
    let mut module_text = fs::read_to_string(&modules).unwrap();
    module_text.push_str("\n[submodule \"missing\"]\n\tpath = missing\n\turl = ../missing\n");
    fs::write(&modules, module_text).unwrap();
    parent.commit_all("add submodule metadata");

    let child = parent.root().join("child");
    git(&child, &["config", "user.name", "Latte Lens Tests"]);
    git(
        &child,
        &["config", "user.email", "latte-lens@example.invalid"],
    );
    write_file(&child, "tracked.txt", "advanced child\n");
    git(&child, &["add", "--all"]);
    git(&child, &["commit", "--quiet", "-m", "advance child"]);
    write_file(&child, "tracked.txt", "dirty inside child\n");

    let mut app = ready_app(parent.root().to_path_buf()).unwrap();
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    assert_eq!(app.changed_count, 2);

    assert!(app.visible_git_rows().iter().any(|row| {
        matches!(row.kind, latte_lens::app::GitRowKind::Pointer(_))
            && row.label == "(submodule pointer)"
    }));
    assert!(app.visible_git_rows().iter().any(|row| {
        row.label == "child"
            && row.detail.contains("pointer changed")
            && row.detail.contains("internal modified")
    }));
    assert!(app.visible_git_rows().iter().any(|row| {
        row.label == "missing"
            && row.detail.contains("submodule placeholder")
            && row.detail.contains("uninitialized")
    }));
    assert!(
        app.content_lines
            .iter()
            .any(|line| line.contains("diff --git a/child b/child"))
    );

    app.handle_key(key(KeyCode::Char('n')));
    settle(&mut app);
    assert!(
        app.content_lines
            .iter()
            .any(|line| line == "+dirty inside child")
    );
    assert!(
        !app.content_lines
            .iter()
            .any(|line| line.contains("a/child b/child"))
    );
}

#[derive(Clone, Copy, Debug)]
enum SubmoduleProjectionState {
    InternalOnly,
    PointerOnly,
    PointerAndInternal,
}

#[test]
fn projected_submodule_change_count_matrix_is_stable_across_collapse_refresh_and_render() {
    for (state, expected_changes, expected_files, expected_pointers) in [
        (SubmoduleProjectionState::InternalOnly, 1, 1, 0),
        (SubmoduleProjectionState::PointerOnly, 1, 0, 1),
        (SubmoduleProjectionState::PointerAndInternal, 2, 1, 1),
    ] {
        let (_parent, mut app) = submodule_projection_fixture(state);

        let file_rows = app
            .visible_git_rows()
            .iter()
            .filter(|row| matches!(row.kind, GitRowKind::Change(_)))
            .count();
        let pointer_rows = app
            .visible_git_rows()
            .iter()
            .filter(|row| matches!(row.kind, GitRowKind::Pointer(_)))
            .count();
        assert_eq!(file_rows, expected_files, "state: {state:?}");
        assert_eq!(pointer_rows, expected_pointers, "state: {state:?}");
        assert_eq!(app.changed_count, expected_changes, "state: {state:?}");
        assert_eq!(app.scope_entry_count(), expected_changes);

        let backend = TestBackend::new(80, 18);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
        let rendered = format!("{:?}", terminal.backend().buffer());
        let expected_label = if expected_changes == 1 {
            "1 change".to_owned()
        } else {
            format!("{expected_changes} changes")
        };
        assert!(rendered.contains(&expected_label), "state: {state:?}");
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|cell| cell.bg == Color::Reset)
        );

        app.handle_key(key(KeyCode::Home));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.visible_git_rows().len(), 1, "state: {state:?}");
        assert_eq!(app.changed_count, expected_changes, "state: {state:?}");
        assert_eq!(app.scope_entry_count(), expected_changes);
        app.handle_key(key(KeyCode::Enter));

        app.handle_key(key(KeyCode::Char('r')));
        settle(&mut app);
        assert_eq!(app.changed_count, expected_changes, "state: {state:?}");
        assert_eq!(app.scope_entry_count(), expected_changes);
    }
}

#[test]
fn app_rejects_files_and_missing_paths() {
    let directory = tempfile::tempdir().unwrap();
    let file = directory.path().join("file.txt");
    fs::write(&file, "not a directory\n").unwrap();

    assert!(
        ready_app(file)
            .err()
            .expect("file path must fail")
            .to_string()
            .contains("is not a directory")
    );
    assert!(
        ready_app(directory.path().join("missing"))
            .err()
            .expect("missing path must fail")
            .to_string()
            .contains("cannot open")
    );
}

#[test]
fn empty_directory_has_no_selection_and_safe_navigation() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = ready_app(directory.path().to_path_buf()).unwrap();

    assert!(app.all_entries.is_empty());
    assert!(app.selected_entry().is_none());
    assert_eq!(app.content_lines, ["This directory is empty."]);
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::End));
    app.handle_key(key(KeyCode::Enter));
    assert!(app.selected_entry().is_none());
    assert_eq!(app.focused_pane, FocusPane::Tree);
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.focused_pane, FocusPane::ScopeTabs);
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.focused_pane, FocusPane::Tree);
}

#[test]
fn directory_selection_summarizes_nested_changes_and_refresh_preserves_selection() {
    let fixture = TestRepo::new();
    fixture.write("src/lib.rs", "before\n");
    fixture.commit_all("initial");
    fixture.write("src/lib.rs", "after\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("src")));
    assert_eq!(
        app.content_lines,
        [
            "1 changed file in this directory.",
            "",
            "Collapsed · Enter or click to expand."
        ]
    );
    app.handle_key(key(KeyCode::Char('r')));
    settle(&mut app);
    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("src")));
    assert!(app.last_error.is_none());
}

#[test]
fn refresh_failure_is_captured_for_the_footer() {
    let fixture = TestRepo::new();
    fixture.write("file.txt", "fixture\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    fs::remove_dir_all(fixture.root()).unwrap();

    app.handle_key(key(KeyCode::Char('r')));
    settle(&mut app);
    assert!(
        app.last_error
            .as_deref()
            .unwrap()
            .contains("refresh failed")
    );
}

#[test]
fn selecting_a_removed_untracked_file_surfaces_diff_error() {
    let fixture = TestRepo::new();
    fixture.write("a.txt", "tracked\n");
    fixture.commit_all("initial");
    fixture.write("z-untracked.txt", "temporary\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    app.handle_key(key(KeyCode::End));
    fs::remove_file(fixture.root().join("z-untracked.txt")).unwrap();

    app.handle_key(key(KeyCode::Char('d')));
    settle(&mut app);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("z-untracked.txt"))
    );
    assert!(app.content_lines[0].contains("Unable to load diff"));
}

#[test]
fn pane_transfers_and_content_arrows_do_not_change_tree_selection() {
    let fixture = TestRepo::new();
    fixture.write("a.txt", "before\n");
    fixture.write("b.txt", "before\n");
    fixture.commit_all("initial");
    fixture.write("b.txt", "after\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    app.handle_key(key(KeyCode::Char('j')));
    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("b.txt")));
    app.handle_key(key(KeyCode::Char('d')));
    app.handle_key(key(KeyCode::Char('l')));
    assert_eq!(app.focused_pane, FocusPane::Content);
    let selected = app.selected_relative_path();

    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.content_scroll, 1);
    assert_eq!(app.selected_relative_path(), selected);
    assert_eq!(app.focused_pane, FocusPane::Content);
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.content_scroll, 0);
    assert_eq!(app.selected_relative_path(), selected);

    app.handle_key(key(KeyCode::PageDown));
    assert_eq!(app.content_scroll, 12);
    app.handle_key(key(KeyCode::PageUp));
    assert_eq!(app.content_scroll, 0);

    app.handle_key(modified_key(KeyCode::Right, KeyModifiers::SHIFT));
    assert_eq!(app.content_horizontal_scroll, 0);
    app.handle_key(key(KeyCode::Right));
    assert_eq!(app.focused_pane, FocusPane::Content);
    assert_eq!(app.content_horizontal_scroll, 0);
    assert_eq!(app.selected_relative_path(), selected);
    app.handle_key(key(KeyCode::Left));
    assert_eq!(app.focused_pane, FocusPane::Tree);
    assert_eq!(app.content_horizontal_scroll, 0);
    assert_eq!(app.selected_relative_path(), selected);
    app.handle_key(key(KeyCode::Right));
    assert_eq!(app.focused_pane, FocusPane::Content);
    app.handle_key(modified_key(KeyCode::Left, KeyModifiers::SHIFT));
    assert_eq!(app.content_horizontal_scroll, 0);
    assert_eq!(app.selected_relative_path(), selected);

    app.handle_key(key(KeyCode::Char('h')));
    assert_eq!(app.focused_pane, FocusPane::Tree);
    app.handle_key(key(KeyCode::Esc));
    assert!(!app.should_quit());
    app.handle_key(key(KeyCode::Esc));
    assert!(app.should_quit());
}

#[test]
fn error_footer_renders_without_hiding_split_panes() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("plain.txt"), "hello\n").unwrap();
    let mut app = ready_app(directory.path().to_path_buf()).unwrap();
    app.last_error = Some("refresh failed: fixture".to_owned());

    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("refresh failed: fixture"));
    assert!(rendered.contains("Files"));
    assert!(rendered.contains("Preview"));
}

#[test]
fn refresh_and_content_loading_are_visible_without_blocking_navigation() {
    let fixture = TestRepo::new();
    fixture.write("a.txt", "a\n");
    fixture.write("b.txt", "b\n");
    fixture.commit_all("initial");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    let snapshot = app.all_entries.clone();

    app.handle_key(key(KeyCode::Char('r')));
    app.handle_key(key(KeyCode::Down));
    assert!(app.is_refreshing());
    assert!(app.is_content_loading());
    assert_eq!(app.all_entries, snapshot);
    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("b.txt")));

    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("Working"));
    assert!(rendered.contains("LOADING"));
    assert!(rendered.contains("Refreshing workspace"));
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .all(|cell| cell.bg == Color::Reset)
    );

    app.handle_key(key(KeyCode::Esc));
    assert!(!app.should_quit());
    app.handle_key(key(KeyCode::Esc));
    assert!(app.should_quit());
    settle(&mut app);
}

#[test]
fn clean_source_defaults_to_numbered_text_preview() {
    let fixture = TestRepo::new();
    fixture.write("main.rs", "fn main() {\n    println!(\"latte\");\n}\n");
    fixture.commit_all("initial");

    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    assert_eq!(app.content_mode, ContentMode::Preview);
    assert_eq!(app.content_provider.as_deref(), Some("text"));
    assert!(app.content_show_line_numbers);

    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("Preview"));
    assert!(rendered.contains("1 ▾"));
    assert!(rendered.contains("2 │"));
    assert!(rendered.contains("println!"));
}

#[test]
fn go_tabs_render_as_indentation_but_copy_as_original_tabs() {
    let fixture = TestRepo::new();
    fixture.write(
        "main.go",
        "package main\n\nimport (\n\t\"time\"\n)\n\ntype Item struct {\n\tCreatedAt\ttime.Time\n}\n",
    );
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    assert_eq!(app.content_mode, ContentMode::Preview);
    let content_x = app.ui_regions.content_inner.x;
    let row_text = |row: u16| -> String {
        (content_x..app.ui_regions.content_inner.right())
            .filter_map(|column| terminal.backend().buffer().cell((column, row)))
            .map(|cell| cell.symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    };
    let import_row = app.ui_regions.content_inner.y + 3;
    let field_row = app.ui_regions.content_inner.y + 7;
    assert!(row_text(import_row).starts_with("4 │     \"time\""));
    assert!(row_text(field_row).starts_with("8 │     CreatedAt   time.Time"));

    let text_x = content_x + 4;
    app.handle_mouse(mouse_down(text_x + 1, import_row));
    app.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        text_x + 9,
        import_row,
    ));
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        text_x + 9,
        import_row,
    ));
    assert_eq!(app.selected_preview_text().as_deref(), Some("\t\"time\""));

    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    assert!(
        terminal
            .backend()
            .buffer()
            .cell((text_x + 1, import_row))
            .is_some_and(|cell| cell.modifier.contains(Modifier::REVERSED))
    );
}

#[test]
fn preview_wraps_long_lines_with_one_logical_line_number_and_exact_mouse_copy() {
    let fixture = TestRepo::new();
    fixture.write("long.txt", "abcdefghijklmnopqrstuvwxyz0123456789\nsecond\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    let backend = TestBackend::new(60, 12);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    assert_eq!(app.content_mode, ContentMode::Preview);
    assert!(app.content_show_line_numbers);
    assert_eq!(app.ui_regions.content_inner.width, 30);

    let row_text = |row: u16| -> String {
        (app.ui_regions.content_inner.x..app.ui_regions.content_inner.right())
            .filter_map(|column| terminal.backend().buffer().cell((column, row)))
            .map(|cell| cell.symbol())
            .collect()
    };
    let first_row = app.ui_regions.content_inner.y;
    assert_eq!(row_text(first_row), "1 │ abcdefghijklmnopqrstuvwxyz");
    assert!(row_text(first_row + 1).starts_with("  │ 0123456789"));
    assert!(row_text(first_row + 2).starts_with("2 │ second"));

    let text_x = app.ui_regions.content_inner.x + 4;
    app.handle_mouse(mouse_down(text_x + 24, first_row));
    app.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        text_x + 3,
        first_row + 1,
    ));
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        text_x + 3,
        first_row + 1,
    ));
    assert_eq!(app.selected_preview_text().as_deref(), Some("yz0123"));

    app.handle_key(key(KeyCode::End));
    assert_eq!(app.content_scroll, 2);
    app.handle_key(modified_key(KeyCode::Right, KeyModifiers::SHIFT));
    assert_eq!(app.content_horizontal_scroll, 0);
}

#[test]
fn git_diff_wraps_long_lines_and_preserves_mouse_copy() {
    let fixture = TestRepo::new();
    fixture.write("long.txt", "before\n");
    fixture.commit_all("initial");
    fixture.write("long.txt", "abcdefghijklmnopqrstuvwxyz0123456789\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);

    let backend = TestBackend::new(60, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    assert_eq!(app.content_mode, ContentMode::Diff);
    assert_eq!(app.ui_regions.content_inner.width, 30);

    let row_text = |row: u16| -> String {
        (app.ui_regions.content_inner.x..app.ui_regions.content_inner.right())
            .filter_map(|column| terminal.backend().buffer().cell((column, row)))
            .map(|cell| cell.symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    };
    let first_row = (app.ui_regions.content_inner.y..app.ui_regions.content_inner.bottom())
        .find(|&row| row_text(row) == "  1 │ +abcdefghijklmnopqrstuvw")
        .expect("long added diff line should start inside the content panel");
    assert_eq!(row_text(first_row + 1), "    │ xyz0123456789");

    let text_x = app.ui_regions.content_inner.x + 6;
    app.handle_mouse(mouse_down(text_x + 1, first_row + 1));
    app.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        text_x + 9,
        first_row + 1,
    ));
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        text_x + 9,
        first_row + 1,
    ));
    assert_eq!(app.selected_content_text().as_deref(), Some("yz0123456"));

    app.handle_key(modified_key(KeyCode::Right, KeyModifiers::SHIFT));
    assert_eq!(app.content_horizontal_scroll, 0);
}

#[test]
fn git_diff_tabs_keep_patch_markers_and_copy_original_tabs() {
    let fixture = TestRepo::new();
    fixture.write("tabbed.txt", "\tbefore\n");
    fixture.commit_all("initial");
    fixture.write("tabbed.txt", "\tafter\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);

    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    let content_x = app.ui_regions.content_inner.x;
    let row_text = |row: u16| -> String {
        (content_x..app.ui_regions.content_inner.right())
            .filter_map(|column| terminal.backend().buffer().cell((column, row)))
            .map(|cell| cell.symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    };
    let deleted_row = (app.ui_regions.content_inner.y..app.ui_regions.content_inner.bottom())
        .find(|&row| row_text(row).ends_with("-    before"))
        .unwrap();
    let added_row = (app.ui_regions.content_inner.y..app.ui_regions.content_inner.bottom())
        .find(|&row| row_text(row).ends_with("+    after"))
        .unwrap();
    assert!(deleted_row < added_row);

    let text_x = content_x + 6;
    app.handle_mouse(mouse_down(text_x + 2, added_row));
    app.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        text_x + 9,
        added_row,
    ));
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        text_x + 9,
        added_row,
    ));
    assert_eq!(app.selected_content_text().as_deref(), Some("\tafter"));
}

#[test]
fn changed_source_defaults_to_diff_but_can_toggle_preview() {
    let fixture = TestRepo::new();
    fixture.write("main.rs", "fn before() {}\n");
    fixture.commit_all("initial");
    fixture.write("main.rs", "fn after() {}\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    assert_eq!(app.content_mode, ContentMode::Diff);
    assert!(
        app.content_lines
            .iter()
            .any(|line| line == "+fn after() {}")
    );
    app.handle_key(key(KeyCode::Char('p')));
    settle(&mut app);
    assert_eq!(app.content_mode, ContentMode::Preview);
    assert_eq!(app.content_lines, ["fn after() {}"]);
    assert!(app.content_highlights[0].iter().any(|highlight| {
        highlight.kind == HighlightKind::Function
            && app.content_lines[0].get(highlight.range.clone()) == Some("after")
    }));
    app.handle_key(key(KeyCode::Char('d')));
    settle(&mut app);
    assert_eq!(app.content_mode, ContentMode::Diff);
}

#[test]
fn binary_file_explains_how_to_add_a_preview_provider() {
    let fixture = TestRepo::new();
    fixture.write("asset.bin", [b'a', 0, b'b']);
    fixture.commit_all("initial");

    let app = ready_app(fixture.root().to_path_buf()).unwrap();
    assert_eq!(app.content_mode, ContentMode::Preview);
    assert!(app.content_provider.is_none());
    assert!(app.content_lines[0].contains("No preview provider accepted"));
    assert!(app.content_lines[1].contains("PreviewProvider"));
}

#[test]
fn app_accepts_a_custom_pdf_preview_provider() {
    struct FakePdfProvider;

    impl PreviewProvider for FakePdfProvider {
        fn id(&self) -> &'static str {
            "fake-pdf"
        }

        fn preview(&self, request: &PreviewRequest<'_>) -> anyhow::Result<Option<PreviewContent>> {
            if request
                .absolute_path
                .extension()
                .and_then(|value| value.to_str())
                == Some("pdf")
            {
                return Ok(Some(PreviewContent::new(vec!["PDF page one".to_owned()])));
            }
            Ok(None)
        }
    }

    let fixture = TestRepo::new();
    fixture.write("design.pdf", b"%PDF-1.7\0fixture");
    fixture.commit_all("initial");
    let mut registry = PreviewRegistry::with_builtins();
    registry.register(FakePdfProvider);

    let app = ready_app_with_preview_registry(fixture.root().to_path_buf(), registry).unwrap();
    assert_eq!(app.content_mode, ContentMode::Preview);
    assert_eq!(app.content_provider.as_deref(), Some("fake-pdf"));
    assert_eq!(app.content_lines, ["PDF page one"]);
}

#[cfg(unix)]
#[test]
fn all_files_expands_a_directory_symlink_and_previews_a_file_inside_it() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().unwrap();
    let workspace = parent.path().join("workspace");
    fs::create_dir(&workspace).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("linked-framework");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("inner.txt"), "inside-linked-directory-3a5e\n").unwrap();
    symlink(&target, workspace.join("a-linked-dir")).unwrap();

    let mut app = ready_app(workspace).unwrap();

    // The directory symlink is an expandable directory row, not a leaf.
    let link_entry = app
        .all_entries
        .iter()
        .find(|entry| entry.relative == Path::new("a-linked-dir"))
        .cloned()
        .expect("the directory symlink is present in the tree");
    assert!(link_entry.is_dir, "a directory symlink must be expandable");
    assert!(!visible_paths(&app).contains(&PathBuf::from("a-linked-dir/inner.txt")));

    // Expand the link (lazy load) and confirm the target's file becomes visible.
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app);
    assert!(visible_paths(&app).contains(&PathBuf::from("a-linked-dir/inner.txt")));

    // Select the file reached through the link and preview its content.
    app.handle_key(key(KeyCode::Down));
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("a-linked-dir/inner.txt"))
    );
    app.handle_key(key(KeyCode::Char('p')));
    settle(&mut app);
    let content = app.content_lines.join("\n");
    assert_eq!(app.content_provider.as_deref(), Some("text"));
    assert!(content.contains("inside-linked-directory-3a5e"));
}

#[cfg(unix)]
#[test]
fn all_files_follows_an_internal_file_symlink_and_previews_target_content() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().unwrap();
    let target = workspace.path().join("z-target.txt");
    fs::write(&target, "internal-target-content-1d61\n").unwrap();
    symlink("z-target.txt", workspace.path().join("a-link.txt")).unwrap();

    let app = ready_app(workspace.path().to_path_buf()).unwrap();
    let content = app.content_lines.join("\n");

    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("a-link.txt"))
    );
    // All Files follows the link and previews the target file as if it were a
    // regular file: the target content shows and the text provider handles it.
    assert_eq!(app.content_provider.as_deref(), Some("text"));
    assert!(content.contains("internal-target-content-1d61"));
}

#[cfg(unix)]
#[test]
fn all_files_follows_an_external_file_symlink_and_previews_target_content() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let outside_file = outside.path().join("a-secret-notes.txt");
    fs::write(&outside_file, "external-target-content-4c02\n").unwrap();
    symlink(&outside_file, workspace.path().join("a-link.txt")).unwrap();

    let app = ready_app(workspace.path().to_path_buf()).unwrap();
    let content = app.content_lines.join("\n");

    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("a-link.txt"))
    );
    // A file symlink pointing outside the workspace is followed too: All Files
    // is a filesystem view, so the user sees the linked file's content.
    assert_eq!(app.content_provider.as_deref(), Some("text"));
    assert!(content.contains("external-target-content-4c02"));
}

#[cfg(unix)]
#[test]
fn all_files_treats_a_directory_symlink_as_an_expandable_directory() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("AspectCore-Framework");
    fs::create_dir(&target).unwrap();
    fs::write(target.join("README.md"), "directory-target-content-778e").unwrap();
    symlink(&target, workspace.path().join("AspectCore-Framework")).unwrap();

    let mut app = ready_app(workspace.path().to_path_buf()).unwrap();

    // A directory symlink is a directory row: it is expandable and is not
    // rendered through the link-text "symlink" preview provider.
    let link_entry = app
        .visible_entries()
        .iter()
        .find(|entry| entry.relative == Path::new("AspectCore-Framework"))
        .cloned()
        .expect("the directory symlink is present in the tree");
    assert!(
        link_entry.is_dir,
        "a directory symlink must be an expandable directory"
    );
    assert_ne!(app.content_provider.as_deref(), Some("symlink"));

    // Selecting the directory symlink shows its resolved real path in the info
    // pane (directory links never enter Preview mode, so this is where the
    // target surfaces).
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("AspectCore-Framework"))
    );
    app.handle_key(key(KeyCode::Char('p')));
    settle(&mut app);
    let info = app.content_lines.join("\n");
    assert!(
        info.contains('↗'),
        "info pane marks the resolved path with ↗"
    );
    assert!(
        info.contains(&target.canonicalize().unwrap().display().to_string()),
        "info pane shows the directory symlink's real path"
    );
}

#[cfg(unix)]
#[test]
fn all_files_tree_marks_symlinks_with_their_target_and_header_shows_real_path() {
    use std::os::unix::fs::symlink;

    let workspace = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let target = outside.path().join("real-target.txt");
    fs::write(&target, "linked-file-body\n").unwrap();
    symlink(&target, workspace.path().join("a-link.txt")).unwrap();

    let mut app = ready_app(workspace.path().to_path_buf()).unwrap();

    // The tree row surfaces the symlink target after a ⇢ marker. Wide layout so
    // the (short, relative) marker text is not truncated away.
    let backend = TestBackend::new(160, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains('⇢'), "tree marks a symlink with ⇢");
    // The entry itself carries the raw link target regardless of pane width.
    let link_entry = app
        .visible_entries()
        .iter()
        .find(|entry| entry.relative == Path::new("a-link.txt"))
        .cloned()
        .expect("the file symlink is present in the tree");
    assert_eq!(link_entry.symlink_target.as_deref(), Some(target.as_path()));

    // Selecting and previewing the link surfaces its canonical real path in the
    // content header (↗ marker), alongside the followed text content.
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("a-link.txt"))
    );
    app.handle_key(key(KeyCode::Char('p')));
    settle(&mut app);
    let real_path = app
        .selected_symlink_real_path()
        .expect("a selected symlink resolves to a real path");
    assert_eq!(real_path, target.canonicalize().unwrap());

    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(
        rendered.contains('↗'),
        "content header marks the resolved link path with ↗"
    );
}

#[cfg(unix)]
#[test]
fn git_changes_preview_does_not_follow_a_changed_symlink() {
    use std::os::unix::fs::symlink;

    let fixture = TestRepo::new();
    fixture.write("inside.txt", "inside\n");
    symlink("inside.txt", fixture.root().join("link.txt")).unwrap();
    fixture.commit_all("initial symlink");

    let outside = tempfile::tempdir().unwrap();
    let secret = "changed-symlink-external-secret-97b8";
    let outside_file = outside.path().join("secret.txt");
    fs::write(&outside_file, secret).unwrap();
    fs::remove_file(fixture.root().join("link.txt")).unwrap();
    symlink(&outside_file, fixture.root().join("link.txt")).unwrap();

    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    app.handle_key(key(KeyCode::Char('p')));
    settle(&mut app);
    let content = app.content_lines.join("\n");

    assert_eq!(app.content_mode, ContentMode::Preview);
    assert!(content.contains(&outside_file.display().to_string()));
    assert!(!content.contains(secret));
}

#[cfg(unix)]
#[test]
fn fifo_preview_returns_promptly_with_a_safe_message() {
    completes_within("FIFO preview and App drop", || {
        let workspace = tempfile::tempdir().unwrap();
        make_fifo(&workspace.path().join("pipe"));

        let app = ready_app(workspace.path().to_path_buf()).unwrap();
        let content = app.content_lines.join("\n");
        assert!(content.contains("FIFO (named pipe)"));
        assert!(content.contains("reads only regular files"));
        drop(app);
    });
}

#[cfg(unix)]
#[test]
fn untracked_symlink_to_fifo_diff_and_worker_drop_return_promptly() {
    use std::os::unix::fs::symlink;

    completes_within("untracked symlink diff and WorkerRuntime drop", || {
        let fixture = TestRepo::new();
        let outside = tempfile::tempdir().unwrap();
        let fifo = outside.path().join("pipe");
        make_fifo(&fifo);
        symlink(&fifo, fixture.root().join("pipe-link")).unwrap();

        let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
        app.set_tree_scope(TreeScope::GitChanges);
        app.wait_for_background();
        let diff = app.content_lines.join("\n");
        assert!(diff.contains("── UNTRACKED ──"));
        assert!(diff.contains("new file mode 120000"));
        assert!(diff.contains(&format!("+{}", fifo.display())));
        drop(app);
    });
}

#[test]
fn visual_focus_cues_identify_tabs_tree_and_content_without_backgrounds() {
    let fixture = TestRepo::new();
    fixture.write("file.txt", "hello\n");
    fixture.commit_all("initial");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let tree_heading = terminal
        .backend()
        .buffer()
        .cell((app.ui_regions.tree_body.x, app.ui_regions.tree_body.y))
        .unwrap();
    assert_eq!(tree_heading.symbol(), "●");
    assert_eq!(tree_heading.fg, Color::Rgb(200, 184, 224));
    assert!(!tree_heading.modifier.contains(Modifier::REVERSED));
    assert!(!tree_heading.modifier.contains(Modifier::UNDERLINED));
    assert_eq!(tree_heading.bg, Color::Reset);
    let selected_row = terminal
        .backend()
        .buffer()
        .cell((app.ui_regions.tree_inner.x, app.ui_regions.tree_inner.y))
        .unwrap();
    assert_eq!(selected_row.symbol(), "▌");
    assert_eq!(selected_row.bg, Color::Reset);
    assert!(!selected_row.modifier.contains(Modifier::REVERSED));
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("● Files"));
    assert!(rendered.contains("Tree"));
    assert!(!rendered.contains("┌"));
    assert!(!rendered.contains("┐"));

    app.handle_key(key(KeyCode::Char('l')));
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let content_heading = terminal
        .backend()
        .buffer()
        .cell((
            app.ui_regions.content_body.x + 1,
            app.ui_regions.content_body.y,
        ))
        .unwrap();
    assert_eq!(content_heading.symbol(), "●");
    assert_eq!(content_heading.fg, Color::Rgb(200, 184, 224));
    assert!(!content_heading.modifier.contains(Modifier::REVERSED));
    assert!(!content_heading.modifier.contains(Modifier::UNDERLINED));
    let unfocused_tree_heading = terminal
        .backend()
        .buffer()
        .cell((app.ui_regions.tree_body.x, app.ui_regions.tree_body.y))
        .unwrap();
    assert_ne!(unfocused_tree_heading.symbol(), "●");
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("● Preview"));
    assert!(rendered.contains("Content"));

    app.content_scroll = 99;
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("hello"));

    app.handle_key(key(KeyCode::Char('h')));
    app.handle_key(key(KeyCode::Up));
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let active_tab = terminal
        .backend()
        .buffer()
        .cell((
            app.ui_regions.all_files_tab.x,
            app.ui_regions.all_files_tab.y,
        ))
        .unwrap();
    assert_eq!(active_tab.symbol(), "●");
    assert!(active_tab.modifier.contains(Modifier::BOLD));
    assert!(active_tab.modifier.contains(Modifier::UNDERLINED));
    assert!(!active_tab.modifier.contains(Modifier::REVERSED));
    let inactive_tab = terminal
        .backend()
        .buffer()
        .cell((
            app.ui_regions.git_changes_tab.x + 2,
            app.ui_regions.git_changes_tab.y,
        ))
        .unwrap();
    assert_eq!(inactive_tab.symbol(), "2");
    assert!(!inactive_tab.modifier.contains(Modifier::UNDERLINED));
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("● 1 Files"));
    assert!(rendered.contains("Tabs"));
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .all(|cell| cell.bg == Color::Reset)
    );

    let backend = TestBackend::new(200, 20);
    let mut wide_terminal = Terminal::new(backend).unwrap();
    wide_terminal
        .draw(|frame| ui::draw(frame, &mut app))
        .unwrap();
    assert_eq!(app.ui_regions.tree_body.width, 44);
    assert_eq!(app.ui_regions.content_body.x, 45);
}

#[test]
fn all_files_aligns_directory_and_file_labels_with_a_blank_file_disclosure_slot() {
    let fixture = TestRepo::new();
    fixture.write("folder/nested.txt", "nested\n");
    fixture.write("plain.txt", "plain\n");
    fixture.commit_all("initial");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    let directory_row = app
        .visible_entries()
        .iter()
        .position(|entry| entry.relative == Path::new("folder"))
        .unwrap();
    let file_row = app
        .visible_entries()
        .iter()
        .position(|entry| entry.relative == Path::new("plain.txt"))
        .unwrap();
    let directory_y = app.ui_regions.tree_inner.y + u16::try_from(directory_row).unwrap();
    let file_y = app.ui_regions.tree_inner.y + u16::try_from(file_row).unwrap();
    let label_start = |row, first_character| {
        (app.ui_regions.tree_inner.x..app.ui_regions.tree_inner.right())
            .find(|&column| {
                terminal
                    .backend()
                    .buffer()
                    .cell((column, row))
                    .is_some_and(|cell| cell.symbol() == first_character)
            })
            .unwrap()
    };
    let directory_label_x = label_start(directory_y, "f");
    let file_label_x = label_start(file_y, "p");

    assert_eq!(directory_label_x, file_label_x);
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((directory_label_x - 2, directory_y))
            .unwrap()
            .symbol(),
        "▸"
    );
    for column in (file_label_x - 2)..file_label_x {
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((column, file_y))
                .unwrap()
                .symbol(),
            " "
        );
    }
}

#[test]
fn all_files_uses_a_small_aligned_change_gutter() {
    let fixture = TestRepo::new();
    fixture.write("changed-dir/nested.txt", "before nested\n");
    fixture.write("root.txt", "before root\n");
    fixture.commit_all("initial");
    fixture.write("changed-dir/nested.txt", "after nested\n");
    fixture.write("root.txt", "after root\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    let hint_x = app
        .ui_regions
        .tree_inner
        .x
        .saturating_add(app.ui_regions.tree_inner.width)
        .saturating_sub(2);
    let directory_hint = terminal
        .backend()
        .buffer()
        .cell((hint_x, app.ui_regions.tree_inner.y))
        .unwrap();
    assert_eq!(directory_hint.symbol(), "•");
    assert_eq!(UnicodeWidthStr::width(directory_hint.symbol()), 1);
    assert_eq!(directory_hint.fg, Color::Rgb(196, 151, 126));
    assert!(!directory_hint.modifier.contains(Modifier::BOLD));
    assert_eq!(directory_hint.bg, Color::Reset);

    let file_hint = terminal
        .backend()
        .buffer()
        .cell((hint_x, app.ui_regions.tree_inner.y + 1))
        .unwrap();
    assert_eq!(file_hint.symbol(), "ᴍ");
    assert_eq!(UnicodeWidthStr::width(file_hint.symbol()), 1);
    assert_eq!(file_hint.fg, Color::Rgb(196, 151, 126));
    assert!(!file_hint.modifier.contains(Modifier::BOLD));
    assert_eq!(file_hint.bg, Color::Reset);

    let trailing_spacer = terminal
        .backend()
        .buffer()
        .cell((hint_x + 1, app.ui_regions.tree_inner.y))
        .unwrap();
    assert_eq!(trailing_spacer.symbol(), " ");
    assert_eq!(hint_x, app.ui_regions.tree_inner.right() - 2);

    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let changed_row = app
        .visible_git_rows()
        .iter()
        .position(|row| row.status.is_some())
        .unwrap();
    let git_hint = terminal
        .backend()
        .buffer()
        .cell((
            app.ui_regions.tree_inner.right() - 2,
            app.ui_regions.tree_inner.y + u16::try_from(changed_row).unwrap(),
        ))
        .unwrap();
    assert_eq!(git_hint.symbol(), "ᴍ");
    assert_eq!(UnicodeWidthStr::width(git_hint.symbol()), 1);
    assert!(!git_hint.modifier.contains(Modifier::BOLD));
}

#[test]
fn git_repository_change_count_stays_fixed_right_when_details_are_truncated() {
    let fixture = TestRepo::new();
    for index in 0..12 {
        fixture.write(&format!("tracked-{index:02}.txt"), "before\n");
    }
    fixture.commit_all("initial");
    fixture.git(&[
        "checkout",
        "--quiet",
        "-b",
        "feature/a-very-long-branch-name-for-sidebar-layout",
    ]);
    for index in 0..12 {
        fixture.write(&format!("tracked-{index:02}.txt"), "after\n");
    }

    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    let repository_row = app
        .visible_git_rows()
        .iter()
        .position(|row| {
            matches!(
                row.kind,
                GitRowKind::Repository {
                    change_count: 12,
                    ..
                }
            )
        })
        .expect("dirty repository row");
    assert!(
        !app.visible_git_rows()[repository_row]
            .detail
            .contains("12 files")
    );

    let backend = TestBackend::new(80, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    let row_y = app.ui_regions.tree_inner.y + u16::try_from(repository_row).unwrap();
    let count_x = app.ui_regions.tree_inner.right() - 3;
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer.cell((count_x - 1, row_y)).unwrap().symbol(), " ");
    assert_eq!(buffer.cell((count_x, row_y)).unwrap().symbol(), "1");
    assert_eq!(buffer.cell((count_x + 1, row_y)).unwrap().symbol(), "2");
    assert_eq!(buffer.cell((count_x + 2, row_y)).unwrap().symbol(), " ");
    for column in count_x..=count_x + 1 {
        assert_eq!(
            buffer.cell((column, row_y)).unwrap().fg,
            Color::Rgb(196, 151, 126)
        );
    }
    let rendered_row = (app.ui_regions.tree_inner.x..app.ui_regions.tree_inner.right())
        .map(|column| buffer.cell((column, row_y)).unwrap().symbol())
        .collect::<String>();
    assert!(rendered_row.contains('…'));
    assert!(!rendered_row.contains("feature/a-very-long-branch-name-for-sidebar-layout"));
}

#[test]
fn divider_drag_resizes_tree_with_minimum_tree_and_content_widths() {
    let fixture = TestRepo::new();
    fixture.write("file.txt", "hello\n");
    fixture.commit_all("initial");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    assert_eq!(app.ui_regions.tree_body.width, 36);
    let drag_row = app.ui_regions.divider.y + 2;
    app.handle_mouse(mouse_down(app.ui_regions.divider.x, drag_row));
    app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 50, drag_row));
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    assert_eq!(app.ui_regions.tree_body.width, 50);
    assert_eq!(app.ui_regions.divider.x, 50);
    assert_eq!(app.ui_regions.content_body.x, 51);
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((app.ui_regions.divider.x, drag_row))
            .unwrap()
            .symbol(),
        "┃"
    );
    app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 50, drag_row));
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    assert_eq!(
        terminal
            .backend()
            .buffer()
            .cell((app.ui_regions.divider.x, drag_row))
            .unwrap()
            .symbol(),
        "│"
    );

    app.handle_mouse(mouse_down(app.ui_regions.divider.x, drag_row));
    app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 0, drag_row));
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    assert_eq!(app.ui_regions.tree_body.width, 28);
    app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 0, drag_row));

    app.handle_mouse(mouse_down(app.ui_regions.divider.x, drag_row));
    app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 99, drag_row));
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    assert_eq!(app.ui_regions.tree_body.width, 75);
    assert_eq!(app.ui_regions.content_body.width, 24);
    app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 99, drag_row));

    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    assert_eq!(app.ui_regions.tree_body.width, 75);
    assert_eq!(app.ui_regions.content_body.width, 24);
}

#[test]
fn ui_split_layout_uses_terminal_default_background_in_both_scopes() {
    let fixture = TestRepo::new();
    fixture.write("a-clean.txt", "hello\n");
    fixture.write("b-changed.txt", "before\n");
    fixture.commit_all("initial");
    fixture.write("b-changed.txt", "after\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    let backend = TestBackend::new(80, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    assert_eq!(app.ui_regions.tree_body.x, 0);
    assert_eq!(app.ui_regions.content_body.x, 29);
    assert_eq!(app.ui_regions.content_body.width, 51);
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .all(|cell| cell.bg == Color::Reset),
        "the UI must inherit the terminal background"
    );

    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.focused_pane, FocusPane::ScopeTabs);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let focused_tab = terminal
        .backend()
        .buffer()
        .cell((
            app.ui_regions.all_files_tab.x + 2,
            app.ui_regions.all_files_tab.y,
        ))
        .unwrap();
    assert!(focused_tab.modifier.contains(Modifier::UNDERLINED));
    assert_eq!(focused_tab.bg, Color::Reset);

    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("Git changes"));
    assert!(rendered.contains("Diff"));
    assert!(rendered.contains("+after"));
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .all(|cell| cell.bg == Color::Reset)
    );
}

#[test]
fn mouse_switches_scope_selects_rows_and_scrolls_the_pointed_pane() {
    let fixture = TestRepo::new();
    fixture.write("a-clean.txt", "clean\n");
    fixture.write("b-changed.txt", "before b\n");
    fixture.write("c-changed.txt", "before c\n");
    fixture.commit_all("initial");
    fixture.write("b-changed.txt", "after b\n");
    fixture.write("c-changed.txt", "after c\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.focused_pane, FocusPane::ScopeTabs);

    app.handle_mouse(mouse_down(
        app.ui_regions.git_changes_tab.x,
        app.ui_regions.git_changes_tab.y,
    ));
    settle(&mut app);
    assert_eq!(app.tree_scope, TreeScope::GitChanges);
    assert_eq!(app.focused_pane, FocusPane::Tree);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("b-changed.txt"))
    );
    assert_eq!(app.content_mode, ContentMode::Diff);

    app.handle_mouse(mouse_down(
        app.ui_regions.tree_inner.x,
        // Row zero is the selectable repository header.
        app.ui_regions.tree_inner.y + 2,
    ));
    settle(&mut app);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("c-changed.txt"))
    );
    assert!(app.content_lines.iter().any(|line| line == "+after c"));

    app.handle_mouse(mouse(
        MouseEventKind::ScrollDown,
        app.ui_regions.content_inner.x,
        app.ui_regions.content_inner.y,
    ));
    assert_eq!(app.focused_pane, FocusPane::Content);
    assert_eq!(app.content_scroll, 3);

    app.handle_mouse(mouse_down(
        app.ui_regions.all_files_tab.x,
        app.ui_regions.all_files_tab.y,
    ));
    settle(&mut app);
    assert_eq!(app.tree_scope, TreeScope::AllFiles);
    assert_eq!(app.content_mode, ContentMode::Preview);
    app.handle_mouse(mouse(
        MouseEventKind::ScrollDown,
        app.ui_regions.tree_inner.x,
        app.ui_regions.tree_inner.y,
    ));
    assert_eq!(app.focused_pane, FocusPane::Tree);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("c-changed.txt"))
    );
}

#[test]
fn preview_mouse_drag_selects_visible_text_and_ctrl_c_queues_exact_copy() {
    let fixture = TestRepo::new();
    fixture.write("notes.txt", "alpha beta\nsecond line\nthird\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    assert_eq!(app.content_mode, ContentMode::Preview);
    assert!(format!("{:?}", terminal.backend().buffer()).contains("Ctrl+C"));
    let text_x = app.ui_regions.content_inner.x + 4;
    let first_row = app.ui_regions.content_inner.y;
    app.handle_mouse(mouse_down(text_x + 6, first_row));
    app.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        text_x + 10,
        first_row + 1,
    ));
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        text_x + 10,
        first_row + 1,
    ));

    assert_eq!(app.focused_pane, FocusPane::Content);
    assert_eq!(
        app.selected_preview_text().as_deref(),
        Some("beta\nsecond line")
    );
    assert_eq!(
        app.clipboard_status.as_deref(),
        Some("Copying 16 characters…")
    );
    app.handle_key(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(!app.should_quit());
    assert_eq!(
        app.clipboard_status.as_deref(),
        Some("Copying 16 characters…")
    );
    app.handle_key(modified_key(
        KeyCode::Char('C'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    assert_eq!(
        app.clipboard_status.as_deref(),
        Some("Copying 16 characters…")
    );
    app.handle_key(modified_key(KeyCode::Char('c'), KeyModifiers::SUPER));
    assert_eq!(
        app.clipboard_status.as_deref(),
        Some("Copying 16 characters…")
    );

    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let first_selected_cell = terminal
        .backend()
        .buffer()
        .cell((text_x + 6, first_row))
        .unwrap();
    assert_eq!(first_selected_cell.symbol(), "b");
    assert!(first_selected_cell.modifier.contains(Modifier::REVERSED));
    let line_number = terminal
        .backend()
        .buffer()
        .cell((app.ui_regions.content_inner.x, first_row))
        .unwrap();
    assert!(!line_number.modifier.contains(Modifier::REVERSED));
    assert!(format!("{:?}", terminal.backend().buffer()).contains("Copying 16 characters"));

    app.handle_mouse(mouse_down(
        app.ui_regions.tree_inner.x,
        app.ui_regions.tree_inner.y,
    ));
    assert!(app.selected_preview_text().is_none());
}

#[test]
fn default_diff_content_can_be_mouse_selected_and_copied() {
    let fixture = TestRepo::new();
    fixture.write("changed.txt", "before\n");
    fixture.commit_all("initial");
    fixture.write("changed.txt", "after\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    assert_eq!(app.content_mode, ContentMode::Diff);
    assert!(app.content_show_line_numbers);
    let column = app.ui_regions.content_inner.x;
    let row_text = |row: u16| -> String {
        (column..app.ui_regions.content_inner.right())
            .filter_map(|x| terminal.backend().buffer().cell((x, row)))
            .map(|cell| cell.symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    };
    let deleted_row = (app.ui_regions.content_inner.y..app.ui_regions.content_inner.bottom())
        .find(|&row| row_text(row).ends_with("-before"))
        .unwrap();
    let added_row = (app.ui_regions.content_inner.y..app.ui_regions.content_inner.bottom())
        .find(|&row| row_text(row).ends_with("+after"))
        .unwrap();
    let deleted_number = terminal
        .backend()
        .buffer()
        .cell((column, deleted_row))
        .unwrap();
    assert_eq!(deleted_number.symbol(), "1");
    assert_eq!(deleted_number.fg, Color::LightRed);
    assert!(deleted_number.modifier.contains(Modifier::BOLD));
    let added_number = terminal
        .backend()
        .buffer()
        .cell((column + 2, added_row))
        .unwrap();
    assert_eq!(added_number.symbol(), "1");
    assert_eq!(added_number.fg, Color::LightGreen);
    assert!(added_number.modifier.contains(Modifier::BOLD));
    let added_marker = terminal
        .backend()
        .buffer()
        .cell((column + 6, added_row))
        .unwrap();
    assert_eq!(added_marker.symbol(), "+");
    assert_eq!(added_marker.fg, Color::LightGreen);
    assert!(added_marker.modifier.contains(Modifier::BOLD));

    app.handle_mouse(mouse_down(column + 7, added_row));
    app.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        column + 11,
        added_row,
    ));
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        column + 11,
        added_row,
    ));

    assert_eq!(app.selected_content_text().as_deref(), Some("after"));
    app.handle_key(modified_key(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert_eq!(
        app.clipboard_status.as_deref(),
        Some("Copying 5 characters…")
    );

    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let selected = terminal
        .backend()
        .buffer()
        .cell((column + 7, added_row))
        .unwrap();
    assert_eq!(selected.symbol(), "a");
    assert!(selected.modifier.contains(Modifier::REVERSED));
}

#[test]
fn info_mouse_selection_maps_the_visual_inset_to_the_exact_content_row() {
    let fixture = TestRepo::new();
    fixture.write("src/lib.rs", "before\n");
    fixture.commit_all("initial");
    fixture.write("src/lib.rs", "after\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    assert_eq!(app.content_mode, ContentMode::Info);
    assert_eq!(
        app.content_lines,
        [
            "1 changed file in this directory.",
            "",
            "Collapsed \u{b7} Enter or click to expand."
        ]
    );

    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    let column = app.ui_regions.content_inner.x;
    // Info content is rendered one row below content_inner, so line index 2 is
    // visible at y + 3 rather than y + 2.
    let row = app.ui_regions.content_inner.y + 3;
    app.handle_mouse(mouse_down(column, row));
    app.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        column + 8,
        row,
    ));
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        column + 8,
        row,
    ));

    assert_eq!(app.selected_content_text().as_deref(), Some("Collapsed"));
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let selected = terminal.backend().buffer().cell((column, row)).unwrap();
    assert_eq!(selected.symbol(), "C");
    assert!(selected.modifier.contains(Modifier::REVERSED));
    let row_above = terminal.backend().buffer().cell((column, row - 1)).unwrap();
    assert!(!row_above.modifier.contains(Modifier::REVERSED));
}

#[test]
fn clickable_refresh_control_updates_the_current_git_changes_tree() {
    let fixture = TestRepo::new();
    fixture.write("clean.txt", "clean\n");
    fixture.commit_all("initial");
    fixture.write("first-change.txt", "first\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);

    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    fixture.write("second-change.txt", "second\n");

    app.handle_mouse(mouse_down(
        app.ui_regions.refresh_button.x,
        app.ui_regions.refresh_button.y,
    ));
    settle(&mut app);

    assert_eq!(app.tree_scope, TreeScope::GitChanges);
    assert_eq!(app.changed_count, 2);
    let changed_paths: Vec<_> = app
        .visible_entries()
        .iter()
        .map(|entry| entry.relative.clone())
        .collect();
    assert_eq!(
        changed_paths,
        [
            PathBuf::from("first-change.txt"),
            PathBuf::from("second-change.txt")
        ]
    );
    assert!(
        app.last_error.is_none(),
        "refresh left an application error: {:?}",
        app.last_error
    );
}

#[test]
fn diff_mode_cycles_between_changed_files() {
    let fixture = TestRepo::new();
    fixture.write("a.txt", "before a\n");
    fixture.write("b.txt", "clean\n");
    fixture.write("c.txt", "before c\n");
    fixture.commit_all("initial");
    fixture.write("a.txt", "after a\n");
    fixture.write("c.txt", "after c\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("a.txt")));
    app.handle_key(key(KeyCode::Char('n')));
    settle(&mut app);
    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("c.txt")));
    assert!(app.content_lines.iter().any(|line| line == "+after c"));
    app.handle_key(key(KeyCode::Char('N')));
    settle(&mut app);
    assert_eq!(app.selected_relative_path(), Some(PathBuf::from("a.txt")));
}

#[test]
fn diff_rows_show_line_stats_and_reviewed_versions_become_stale_after_refresh() {
    let fixture = TestRepo::new();
    fixture.write("review.txt", "old one\nold two\n");
    fixture.commit_all("initial");
    fixture.write("review.txt", "new one\nold two\nadded\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    let render = |app: &mut App| {
        let backend = TestBackend::new(120, 22);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| ui::draw(frame, app)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };

    let initial = render(&mut app);
    assert!(initial.contains('○'));
    assert!(initial.contains("+2"));
    assert!(initial.contains("-1"));
    assert!(initial.contains("0/1 reviewed"));

    app.handle_key(key(KeyCode::Char(' ')));
    let reviewed = render(&mut app);
    assert!(reviewed.contains('✓'));
    assert!(reviewed.contains("1/1 reviewed"));

    app.handle_key(key(KeyCode::Char('r')));
    settle(&mut app);
    assert!(render(&mut app).contains('✓'));

    fixture.write(
        "review.txt",
        "newer first line\nold two\nadded\nsecond addition\n",
    );
    app.handle_key(key(KeyCode::Char('r')));
    settle(&mut app);
    let changed = render(&mut app);
    assert!(changed.contains('↻'));
    assert!(changed.contains("+3"));
    assert!(changed.contains("-1"));
    assert!(changed.contains("0/1 reviewed"));
    assert!(changed.contains("1 changed"));

    app.handle_key(key(KeyCode::Char(' ')));
    assert!(render(&mut app).contains('✓'));
    app.handle_key(key(KeyCode::Char(' ')));
    assert!(render(&mut app).contains('○'));
}

#[test]
fn staged_content_change_invalidates_review_even_when_line_counts_match() {
    let fixture = TestRepo::new();
    fixture.write("staged.txt", "before\n");
    fixture.commit_all("initial");
    fixture.write("staged.txt", "after one\n");
    fixture.git(&["add", "staged.txt"]);
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    app.handle_key(key(KeyCode::Char(' ')));

    fixture.write("staged.txt", "after two\n");
    fixture.git(&["add", "staged.txt"]);
    app.handle_key(key(KeyCode::Char('r')));
    settle(&mut app);

    let backend = TestBackend::new(120, 22);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(rendered.contains('↻'));
    assert!(rendered.contains("+1"));
    assert!(rendered.contains("-1"));
}

#[test]
fn file_search_filters_previews_and_reveals_collapsed_paths() {
    let fixture = TestRepo::new();
    fixture.write("docs/readme.md", "documentation\n");
    fixture.write("src/app_controller.rs", "pub fn controller() {}\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    app.handle_key(key(KeyCode::Char('/')));
    assert_eq!(app.search_mode(), Some(SearchMode::Files));
    for character in "appc".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    settle(&mut app);

    assert_eq!(app.search_results().len(), 1);
    assert_eq!(
        app.selected_search_result()
            .map(|result| result.path.as_path()),
        Some(Path::new("src/app_controller.rs"))
    );
    assert!(
        app.content_lines
            .iter()
            .any(|line| line.contains("controller"))
    );

    app.handle_key(key(KeyCode::Enter));
    settle(&mut app);
    assert!(!app.search_is_active());
    assert_eq!(app.tree_scope, TreeScope::AllFiles);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("src/app_controller.rs"))
    );
    assert!(visible_paths(&app).contains(&PathBuf::from("src/app_controller.rs")));
}

#[test]
fn search_popup_preserves_each_mode_until_the_user_clears_it() {
    let fixture = TestRepo::new();
    fixture.write("src/app.rs", "pub fn needle_app() {}\n");
    fixture.write("src/app_controller.rs", "pub fn needle_controller() {}\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    app.handle_key(key(KeyCode::Char('/')));
    for character in "app".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    app.handle_key(key(KeyCode::Down));
    let selected_file = app
        .selected_search_result()
        .map(|result| result.path.clone())
        .unwrap();
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app);
    assert!(!app.search_is_active());

    app.handle_key(modified_key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert_eq!(app.search_mode(), Some(SearchMode::Files));
    assert_eq!(app.search_query(), Some("app"));
    assert_eq!(
        app.selected_search_result()
            .map(|result| result.path.as_path()),
        Some(selected_file.as_path())
    );

    app.handle_key(modified_key(
        KeyCode::Char('F'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    assert_eq!(app.search_mode(), Some(SearchMode::Text));
    assert_eq!(app.search_query(), Some(""));
    for character in "needle".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    settle(&mut app);
    assert_eq!(app.search_results().len(), 2);
    app.handle_key(key(KeyCode::Esc));

    app.handle_key(modified_key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert_eq!(app.search_query(), Some("app"));
    app.handle_key(key(KeyCode::Esc));
    app.handle_key(modified_key(
        KeyCode::Char('F'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    assert_eq!(app.search_query(), Some("needle"));
    assert_eq!(app.search_results().len(), 2);

    app.handle_key(modified_key(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(app.search_query(), Some(""));
    assert!(app.search_results().is_empty());
}

#[test]
fn search_popup_is_wider_than_the_tree_and_keeps_the_terminal_background() {
    let fixture = TestRepo::new();
    fixture.write("src/lib.rs", "pub fn searchable_library() {}\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();

    app.handle_key(key(KeyCode::Char('/')));
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("Open File"));
    assert!(app.ui_regions.search_popup.width > app.ui_regions.tree_body.width);
    assert!(app.ui_regions.search_results.width > app.ui_regions.tree_inner.width);
    assert!(
        terminal
            .backend()
            .buffer()
            .cell((0, 0))
            .is_some_and(|cell| cell.modifier.contains(Modifier::DIM))
    );
    assert!(
        terminal
            .backend()
            .buffer()
            .cell((app.ui_regions.search_popup.x, app.ui_regions.search_popup.y))
            .is_some_and(|cell| !cell.modifier.contains(Modifier::DIM))
    );
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .all(|cell| cell.bg == Color::Reset)
    );
}

#[test]
fn ctrl_t_opens_workspace_search_when_the_terminal_cannot_report_ctrl_shift_f() {
    let fixture = TestRepo::new();
    fixture.write("src/lib.rs", "searchable\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    app.handle_key(modified_key(KeyCode::Char('t'), KeyModifiers::CONTROL));

    assert_eq!(app.search_mode(), Some(SearchMode::Text));
}

#[test]
fn a_refresh_reuses_the_saved_text_query_but_updates_its_results() {
    let fixture = TestRepo::new();
    fixture.write("first.txt", "needle one\n");
    fixture.write("second.txt", "nothing yet\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    app.handle_key(modified_key(
        KeyCode::Char('F'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    for character in "needle".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    settle(&mut app);
    assert_eq!(app.search_results().len(), 1);
    let selected_before_refresh = app
        .selected_search_result()
        .map(|result| result.path.clone())
        .unwrap();
    app.handle_key(key(KeyCode::Esc));

    fixture.write("second.txt", "needle two\n");
    app.handle_key(key(KeyCode::Char('r')));
    settle(&mut app);
    app.handle_key(modified_key(
        KeyCode::Char('F'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    settle(&mut app);

    assert_eq!(app.search_query(), Some("needle"));
    assert_eq!(app.search_results().len(), 2);
    assert_eq!(
        app.selected_search_result()
            .map(|result| result.path.as_path()),
        Some(selected_before_refresh.as_path())
    );
}

#[test]
fn opening_a_search_result_keeps_changed_files_in_git_and_reveals_clean_files_in_files() {
    let fixture = TestRepo::new();
    fixture.write("changed.txt", "before changed-marker\n");
    fixture.write("clean.txt", "clean-marker\n");
    fixture.commit_all("initial");
    fixture.write("changed.txt", "after changed-marker\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    app.handle_key(modified_key(
        KeyCode::Char('F'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    for character in "changed-marker".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    settle(&mut app);
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app);
    assert_eq!(app.tree_scope, TreeScope::GitChanges);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("changed.txt"))
    );

    app.handle_key(modified_key(
        KeyCode::Char('F'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    app.handle_key(modified_key(KeyCode::Char('u'), KeyModifiers::CONTROL));
    for character in "clean-marker".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    settle(&mut app);
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app);
    assert_eq!(app.tree_scope, TreeScope::AllFiles);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("clean.txt"))
    );
}

#[test]
fn text_search_streams_safe_matches_toggles_ignored_and_restores_on_escape() {
    let fixture = TestRepo::new();
    fixture.write(".gitignore", "ignored/\n");
    fixture.write("src/visible.rs", "first\nNeedle visible\nlast\n");
    fixture.write("ignored/hidden.rs", "Needle hidden\n");
    fixture.write("binary.bin", b"Needle\0binary");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    let original_lines = app.content_lines.clone();

    app.handle_key(modified_key(
        KeyCode::Char('F'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    for character in "needle".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    settle(&mut app);
    assert_eq!(app.search_mode(), Some(SearchMode::Text));
    assert_eq!(app.search_results().len(), 1);
    assert_eq!(app.search_results()[0].path, Path::new("src/visible.rs"));
    assert_eq!(app.search_results()[0].line_number, Some(2));
    assert!(
        app.content_highlights
            .get(1)
            .is_some_and(|spans| spans.iter().any(|span| span.kind == HighlightKind::Search))
    );

    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    assert!(format!("{:?}", terminal.backend().buffer()).contains("last Refresh"));

    app.handle_key(key(KeyCode::F(5)));
    settle(&mut app);
    assert_eq!(app.search_results().len(), 2);
    assert!(
        app.search_results()
            .iter()
            .any(|result| result.path == Path::new("ignored/hidden.rs"))
    );
    assert!(
        app.search_results()
            .iter()
            .all(|result| result.path != Path::new("binary.bin"))
    );

    app.handle_key(key(KeyCode::Esc));
    assert!(!app.search_is_active());
    assert_eq!(app.content_lines, original_lines);
}

#[test]
fn preview_find_highlights_navigates_and_hands_off_to_workspace_search() {
    let fixture = TestRepo::new();
    fixture.write(
        "example.ts",
        "const Needle = 1;\nconst other = 2;\nconst needle = 3;\nNeedle();\n",
    );
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    settle(&mut app);

    assert_eq!(app.content_mode, ContentMode::Preview);
    app.handle_key(modified_key(KeyCode::Char('f'), KeyModifiers::CONTROL));
    assert!(app.preview_find_is_active());
    assert!(!app.search_is_active());
    for character in "needle".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }

    assert_eq!(app.preview_find_query(), Some("needle"));
    assert_eq!(app.preview_find_position(), Some((1, 3)));
    assert!(
        app.preview_find_highlights(0)
            .iter()
            .any(|span| span.kind == HighlightKind::Search)
    );
    assert!(
        app.preview_find_highlights(2)
            .iter()
            .any(|span| span.kind == HighlightKind::SearchMatch)
    );

    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.preview_find_position(), Some((2, 3)));
    assert!(
        app.preview_find_highlights(2)
            .iter()
            .any(|span| span.kind == HighlightKind::Search)
    );

    app.handle_key(key(KeyCode::F(2)));
    assert_eq!(app.preview_find_position(), Some((1, 1)));
    assert!(
        app.preview_find_highlights(2)
            .iter()
            .any(|span| span.kind == HighlightKind::Search)
    );

    app.handle_key(modified_key(
        KeyCode::Char('F'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    ));
    assert!(!app.preview_find_is_active());
    assert_eq!(app.search_mode(), Some(SearchMode::Text));
}

#[test]
fn ctrl_f_finds_in_the_current_diff_instead_of_opening_workspace_search() {
    let fixture = TestRepo::new();
    fixture.write("changed.txt", "before needle\n");
    fixture.commit_all("initial");
    fixture.write("changed.txt", "after needle\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    app.handle_key(key(KeyCode::Char('d')));
    settle(&mut app);
    assert_eq!(app.content_mode, ContentMode::Diff);
    app.handle_key(modified_key(KeyCode::Char('f'), KeyModifiers::CONTROL));
    for character in "needle".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }

    assert!(app.preview_find_is_active());
    assert!(!app.search_is_active());
    assert!(
        app.preview_find_position()
            .is_some_and(|(_, count)| count > 0)
    );
}

#[test]
fn preview_find_mouse_controls_move_and_close_without_backgrounds() {
    let fixture = TestRepo::new();
    fixture.write("file.txt", "match one\nmatch two\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    settle(&mut app);
    app.open_preview_find();
    for character in "match".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("Find"));
    assert!(rendered.contains("1/2"));
    let next = app.ui_regions.preview_find_next;
    app.handle_mouse(mouse_down(next.x, next.y));
    assert_eq!(app.preview_find_position(), Some((2, 2)));

    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let close = app.ui_regions.preview_find_close;
    app.handle_mouse(mouse_down(close.x, close.y));
    assert!(!app.preview_find_is_active());
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .all(|cell| cell.bg == Color::Reset)
    );
}

#[test]
fn preview_find_can_open_while_the_selected_file_is_loading() {
    let fixture = TestRepo::new();
    fixture.write("a.txt", "first file\n");
    fixture.write("b.txt", "loading-safe needle\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();

    app.handle_key(key(KeyCode::End));
    assert_eq!(app.content_mode, ContentMode::Preview);
    assert!(app.is_content_loading());
    app.handle_key(modified_key(KeyCode::Char('f'), KeyModifiers::CONTROL));
    for character in "needle".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }
    settle(&mut app);

    assert!(app.preview_find_is_active());
    assert!(!app.search_is_active());
    assert_eq!(app.preview_find_position(), Some((1, 1)));
}

#[test]
fn search_mouse_buttons_open_switch_and_close_without_backgrounds() {
    let fixture = TestRepo::new();
    fixture.write("src/lib.rs", "pub fn library() {}\n");
    let mut app = ready_app(fixture.root().to_path_buf()).unwrap();
    let backend = TestBackend::new(100, 20);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("Open"));
    assert!(rendered.contains("Text"));
    let file_button = app.ui_regions.file_search_button;
    assert!(file_button.width > 0);
    app.handle_mouse(mouse_down(file_button.x, file_button.y));
    assert_eq!(app.search_mode(), Some(SearchMode::Files));

    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let text_mode = app.ui_regions.search_text_mode;
    app.handle_mouse(mouse_down(text_mode.x, text_mode.y));
    assert_eq!(app.search_mode(), Some(SearchMode::Text));
    for character in "library".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }

    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("Aa"));
    assert!(rendered.contains("Word"));
    assert!(rendered.contains(".*"));
    assert!(rendered.contains("Ign"));
    let clear = app.ui_regions.search_clear;
    assert!(clear.width > 0);
    app.handle_mouse(mouse_down(clear.x, clear.y));
    assert_eq!(app.search_query(), Some(""));
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let close = app.ui_regions.search_close;
    app.handle_mouse(mouse_down(close.x, close.y));
    assert!(!app.search_is_active());
    assert!(
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .all(|cell| cell.bg == Color::Reset)
    );
}

#[cfg(feature = "agent-observability")]
#[test]
fn agents_scope_renders_metadata_live_and_explain_states_from_the_view_model() {
    use std::sync::Arc;

    let directory = tempfile::tempdir().unwrap();
    let mut app = ready_app(directory.path().to_path_buf()).unwrap();
    let (handle, endpoint) = agent_runtime_channel(8, 8);
    let workspace = WorkspaceHint::from_digest(digest(3));
    let subject = SubjectNamespace::parse("synthetic/agent").unwrap();
    let session = SessionRef::new(
        SessionKey::new(
            subject.clone(),
            InstallId::from_digest(digest(4)),
            AuthorityId::from_digest(digest(5)),
            digest(6),
        ),
        workspace.clone(),
    );
    app.attach_agent_runtime(
        AgentRuntime::from_channel(handle),
        WorkspaceSelector::new(BoundedVec::try_from_vec(vec![workspace.clone()]).unwrap()),
    )
    .unwrap();
    assert!(endpoint.try_request().is_some());
    assert!(endpoint.try_request().is_some());
    endpoint
        .complete(AgentRuntimeCompletion::MetadataLoaded {
            generation: 1,
            snapshot: MetadataSnapshot {
                workspaces: BoundedVec::new(),
                sessions: BoundedVec::try_from_vec(vec![SessionMetadata {
                    session: session.clone(),
                    observers: BoundedVec::try_from_vec(vec![
                        ObserverId::parse("synthetic/hook").unwrap(),
                    ])
                    .unwrap(),
                    observers_truncated: false,
                    discovery: SessionDiscovery::DiscoveredMidSession,
                    first_observed_at: Timestamp::from_unix_millis(1),
                    last_observed_at: Timestamp::from_unix_millis(2),
                    lifecycle_hint: SessionLifecycleHint::Open,
                    last_activity_hint: ActivityStateHint::Working,
                    last_event_kind: ObservationKindTag::Activity,
                    known_agents: BoundedVec::new(),
                    agents_truncated: false,
                    start_observed: false,
                    terminal: None,
                    generation: 1,
                    partial: true,
                    revived: false,
                }])
                .unwrap(),
                truncated: false,
                corrupt_records_ignored: 0,
            },
        })
        .unwrap();
    app.poll_background();
    app.set_tree_scope(TreeScope::Agents);

    let backend = TestBackend::new(120, 36);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("3 Agents"));
    assert!(rendered.contains("0/1 live"));
    assert!(rendered.contains("synthetic/agent"));
    assert!(rendered.contains("Unknown"));
    assert!(rendered.contains("metadata"));
    assert!(!rendered.contains("metadata Working"));
    assert!(rendered.contains("1/2/3 scope"));

    let observer = ObserverId::parse("synthetic/hook").unwrap();
    let instance = ObserverInstanceId::from_digest(digest(10));
    let epoch = StreamEpoch::from_digest(digest(11));
    let claim = CapabilityClaim {
        support: CapabilitySupport::Confirmed,
        max_authority: EvidenceAuthority::Authoritative,
        provenance: EvidenceProvenance::InstrumentedHook,
        reason: BoundedText::try_new("synthetic UI").unwrap(),
        lease_backed: false,
    };
    let subjects = BoundedSet::try_from_iter([subject]).unwrap();
    let acquisition = BoundedSet::try_from_iter([AcquisitionMode::HookEvent]).unwrap();
    let capabilities = std::collections::BTreeMap::from([(EvidenceDomain::Activity, claim)]);
    let contract = InstanceContract {
        observer: observer.clone(),
        instance: instance.clone(),
        revision: ContractRevision::new(1),
        observer_version: None,
        subjects: subjects.clone(),
        acquisition: acquisition.clone(),
        capabilities: capabilities.clone(),
        snapshot_semantics: SnapshotSemantics::unsupported(),
        stream_semantics: StreamSemantics::unsupported(),
        requires_instrumentation: true,
        stability: InterfaceStability::Stable,
    };
    let template = InstanceContractTemplate {
        observer: observer.clone(),
        subjects,
        acquisition,
        capabilities,
        snapshot_semantics: SnapshotSemantics::unsupported(),
        stream_semantics: StreamSemantics::unsupported(),
        requires_instrumentation: true,
        stability: InterfaceStability::Stable,
    };
    let observation = AgentObservation {
        observed_at: Timestamp::from_unix_millis(3),
        valid_until: None,
        presence: None,
        session: Some(session),
        agent: None,
        turn: None,
        workspace: Some(workspace),
        kind: ObservationKind::Activity(ActivityOp::Set(ReportedActivityState::Working)),
        evidence: EvidenceClaim {
            support: CapabilitySupport::Confirmed,
            authority: EvidenceAuthority::Authoritative,
            provenance: EvidenceProvenance::InstrumentedHook,
        },
    };
    let mut registry = AdapterRegistry::new();
    registry
        .register(Arc::new(FakeAdapter::new(
            ObserverDescriptor::new(observer.clone(), "Synthetic", "1").unwrap(),
            template,
            DecodeOutcome::Ignore(IgnoreReason::NoObservableFact),
        )))
        .unwrap();
    let validated = registry
        .validate_envelope(
            ObservationEnvelope::Event(EventEnvelope {
                stream: StreamRef {
                    observer,
                    instance,
                    epoch,
                },
                event_id: EventId::from_digest(digest(12)),
                sequence: Some(StreamSequence::new(1)),
                op: StreamOp::Upsert(BoundedVec::try_from_vec(vec![observation]).unwrap()),
            }),
            &contract,
        )
        .unwrap();
    endpoint
        .complete(AgentRuntimeCompletion::EnvelopeReceived {
            generation: 1,
            workspace_hints: BoundedVec::new(),
            envelope: Box::new(validated),
        })
        .unwrap();
    app.poll_background();
    app.handle_key(key(KeyCode::Enter));
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let rendered = format!("{:?}", terminal.backend().buffer());
    assert!(rendered.contains("1/1 live"));
    assert!(rendered.contains("Working"));
    assert!(rendered.contains("live"));
    assert!(rendered.contains("Explain"));
    assert!(rendered.contains("Authoritative"));
    assert!(rendered.contains("synthetic/hook"));
    assert!(!rendered.contains("prompt-canary"));
}

#[cfg(unix)]
#[test]
#[cfg(feature = "navigation-test-support")]
fn production_spawner_runs_framed_definition_journey() {
    run_production_spawner_framed_journey();
}

#[cfg(windows)]
#[test]
#[cfg(feature = "navigation-test-support")]
fn windows_production_spawner_runs_framed_definition_journey() {
    run_production_spawner_framed_journey();
}

#[cfg(feature = "navigation-test-support")]
fn run_production_spawner_framed_journey() {
    use std::{
        sync::{Mutex, OnceLock},
        time::{Duration, Instant},
    };

    static ENVIRONMENT_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let _environment = ENVIRONMENT_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let container = tempfile::tempdir().unwrap();
    let workspace = container.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    let caller_text = "caller!(); // 😀中\n";
    let target_text = "pub fn 目标😀() {}\n";
    let caller_path = workspace.join("a-caller.rs");
    let target_path = workspace.join("b-target.rs");
    fs::write(&caller_path, caller_text).unwrap();
    fs::write(&target_path, target_text).unwrap();
    let workspace = workspace.canonicalize().unwrap();
    let caller_path = caller_path.canonicalize().unwrap();
    let target_path = target_path.canonicalize().unwrap();
    let trace = container.path().join("framed.trace");
    let release = container.path().join("release-definition");
    let config = container.path().join("latte-lens.jsonc");
    let helper = PathBuf::from(env!("CARGO_BIN_EXE_latte-lens-lsp-test-helper"))
        .canonicalize()
        .unwrap();
    let config_value = serde_json::json!({
        "code_navigation": {
            "enabled": true,
            "languages": {
                "rust": {
                    "enabled": true,
                    "engine": {
                        "type": "language_server",
                        "command": [helper, "framed-lsp"]
                    }
                }
            }
        }
    });
    fs::write(&config, serde_json::to_vec(&config_value).unwrap()).unwrap();
    let config = config.canonicalize().unwrap();

    let root_uri = url::Url::from_file_path(&workspace).unwrap().to_string();
    let caller_uri = url::Url::from_file_path(&caller_path).unwrap().to_string();
    let target_uri = url::Url::from_file_path(&target_path).unwrap().to_string();
    set_test_env("LATTELENS_CONFIG", config.as_os_str());
    set_test_env("LATTELENS_TEST_HELPER_MODE", "framed-lsp");
    set_test_env("LATTELENS_TEST_ROOT_URI", &root_uri);
    set_test_env("LATTELENS_TEST_CALLER_URI", &caller_uri);
    set_test_env("LATTELENS_TEST_TARGET_URI", &target_uri);
    set_test_env("LATTELENS_TEST_CALLER_TEXT", caller_text);
    set_test_env("LATTELENS_TEST_TRACE", trace.as_os_str());
    set_test_env("LATTELENS_TEST_RELEASE", release.as_os_str());

    let loaded = NavigationSettings::load_user_config(&workspace);
    assert!(loaded.warning.is_none(), "{:?}", loaded.warning);
    assert!(loaded.settings.is_enabled());
    let mut app = App::with_options(
        workspace.clone(),
        PreviewRegistry::with_builtins(),
        AppOptions {
            navigation: loaded.settings,
            navigation_config_warning: loaded.warning,
        },
    )
    .unwrap();
    settle(&mut app);
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("a-caller.rs"))
    );
    assert_eq!(app.content_lines, [caller_text.trim_end()]);

    app.handle_key(key(KeyCode::Char('l')));
    app.handle_key(modified_key(KeyCode::Char('d'), KeyModifiers::CONTROL));

    wait_until(Duration::from_secs(5), || {
        app.poll_background();
        trace_contains(&trace, "definition-received")
    });
    assert_eq!(
        app.selected_relative_path(),
        Some(PathBuf::from("a-caller.rs"))
    );
    assert_eq!(app.content_lines, [caller_text.trim_end()]);

    fs::write(&release, b"go").unwrap();
    let target_deadline = Instant::now() + Duration::from_secs(5);
    while {
        app.poll_background();
        app.selected_relative_path() != Some(PathBuf::from("b-target.rs"))
            || app.content_lines != [target_text.trim_end()]
    } {
        if Instant::now() >= target_deadline {
            let backend = TestBackend::new(120, 24);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
            let rendered: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            panic!(
                "target did not commit; selected={:?}, content={:?}, ui={rendered}",
                app.selected_relative_path(),
                app.content_lines
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(app.focused_pane, FocusPane::Content);

    let probe = app.navigation_test_probe();
    let drop_started = Instant::now();
    drop(app);
    assert!(drop_started.elapsed() < Duration::from_secs(4));
    wait_until(Duration::from_secs(2), || {
        trace_contains(&trace, "orderly-exit")
    });
    let report = probe.snapshot();
    assert_eq!(report.sessions_cleaned, 1);
    assert_eq!(report.clean_exits, 1);
    assert_eq!(report.forced_tree_cleanups, 0);
    assert_eq!(report.direct_children_reaped, 1);
    assert_eq!(report.io_threads_joined, 3);
    assert_eq!(report.process_owners_dropped, 1);

    for key in [
        "LATTELENS_CONFIG",
        "LATTELENS_TEST_HELPER_MODE",
        "LATTELENS_TEST_ROOT_URI",
        "LATTELENS_TEST_CALLER_URI",
        "LATTELENS_TEST_TARGET_URI",
        "LATTELENS_TEST_CALLER_TEXT",
        "LATTELENS_TEST_TRACE",
        "LATTELENS_TEST_RELEASE",
    ] {
        remove_test_env(key);
    }
}

#[cfg(feature = "navigation-test-support")]
fn trace_contains(path: &Path, marker: &str) -> bool {
    fs::read_to_string(path).is_ok_and(|contents| contents.lines().any(|line| line == marker))
}

#[cfg(feature = "navigation-test-support")]
fn wait_until(timeout: std::time::Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + timeout;
    while !predicate() {
        if std::time::Instant::now() >= deadline {
            let trace = std::env::var_os("LATTELENS_TEST_TRACE")
                .and_then(|path| fs::read_to_string(path).ok())
                .unwrap_or_else(|| "<no helper trace>".to_owned());
            panic!("timed out after {timeout:?}; helper trace:\n{trace}");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[cfg(feature = "navigation-test-support")]
fn set_test_env(key: &str, value: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: the real-spawner tests serialize their environment mutation and
    // legacy App constructors never read this explicit configuration variable.
    unsafe { std::env::set_var(key, value) };
}

#[cfg(feature = "navigation-test-support")]
fn remove_test_env(key: &str) {
    // SAFETY: paired with the serialized test-only mutation above.
    unsafe { std::env::remove_var(key) };
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn mouse_down(column: u16, row: u16) -> MouseEvent {
    mouse(MouseEventKind::Down(MouseButton::Left), column, row)
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn visible_paths(app: &App) -> Vec<PathBuf> {
    app.visible_entries()
        .iter()
        .map(|entry| entry.relative.clone())
        .collect()
}

fn settle(app: &mut App) {
    app.wait_for_background();
}

fn ready_app(path: PathBuf) -> anyhow::Result<App> {
    let mut app = App::new(path)?;
    settle(&mut app);
    Ok(app)
}

fn ready_app_with_preview_registry(
    path: PathBuf,
    registry: PreviewRegistry,
) -> anyhow::Result<App> {
    let mut app = App::with_preview_registry(path, registry)?;
    settle(&mut app);
    Ok(app)
}

fn submodule_projection_fixture(state: SubmoduleProjectionState) -> (TestRepo, App) {
    let source = TestRepo::new();
    source.write("tracked.txt", "source initial\n");
    source.commit_all("source initial");

    let parent = TestRepo::new();
    let output = Command::new("git")
        .args([
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "--quiet",
        ])
        .arg(source.root())
        .arg("child")
        .current_dir(parent.root())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "submodule add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    parent.commit_all("add child");

    let child = parent.root().join("child");
    git(&child, &["config", "user.name", "Latte Lens Tests"]);
    git(
        &child,
        &["config", "user.email", "latte-lens@example.invalid"],
    );
    match state {
        SubmoduleProjectionState::InternalOnly => {
            write_file(&child, "tracked.txt", "internal only\n");
        }
        SubmoduleProjectionState::PointerOnly => {
            write_file(&child, "tracked.txt", "advanced pointer\n");
            git(&child, &["add", "--all"]);
            git(&child, &["commit", "--quiet", "-m", "advance pointer"]);
        }
        SubmoduleProjectionState::PointerAndInternal => {
            write_file(&child, "tracked.txt", "advanced pointer\n");
            git(&child, &["add", "--all"]);
            git(&child, &["commit", "--quiet", "-m", "advance pointer"]);
            write_file(&child, "tracked.txt", "pointer and internal\n");
        }
    }

    let mut app = ready_app(parent.root().to_path_buf()).unwrap();
    app.set_tree_scope(TreeScope::GitChanges);
    settle(&mut app);
    (parent, app)
}

#[cfg(unix)]
fn make_fifo(path: &Path) {
    let output = Command::new("mkfifo").arg(path).output().unwrap();
    assert!(
        output.status.success(),
        "mkfifo failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
fn completes_within(description: &'static str, operation: impl FnOnce() + Send + 'static) {
    use std::{
        panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
        sync::mpsc::{self, RecvTimeoutError},
        thread,
        time::Duration,
    };

    let (sender, receiver) = mpsc::sync_channel(1);
    let handle = thread::spawn(move || {
        let outcome = catch_unwind(AssertUnwindSafe(operation));
        let _ = sender.send(outcome);
    });
    match receiver.recv_timeout(Duration::from_secs(3)) {
        Ok(outcome) => {
            handle.join().expect("timeout fixture wrapper panicked");
            if let Err(payload) = outcome {
                resume_unwind(payload);
            }
        }
        Err(RecvTimeoutError::Disconnected) => {
            handle.join().expect("timeout fixture panicked");
            panic!("{description} disconnected without reporting a result");
        }
        Err(RecvTimeoutError::Timeout) => {
            panic!("{description} exceeded the 3 second safety bound");
        }
    }
}

fn init_repo(root: &Path) {
    fs::create_dir_all(root).unwrap();
    git(root, &["-c", "init.defaultBranch=main", "init", "--quiet"]);
    git(root, &["config", "user.name", "Latte Lens Tests"]);
    git(
        root,
        &["config", "user.email", "latte-lens@example.invalid"],
    );
}

fn write_file(root: &Path, relative: &str, contents: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, contents).unwrap();
}

fn git(root: &Path, args: &[&str]) {
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

fn git_with_path(root: &Path, before: &[&str], path: &Path, after: &[&str]) {
    let output = Command::new("git")
        .args(before)
        .arg(path)
        .args(after)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
