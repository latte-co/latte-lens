//! Headless integration tests that complement the PTY E2E scenarios by
//! exercising App code paths that are difficult to reach through a terminal
//! (edge cases, error handling, API-level interactions).

mod support;

use std::path::PathBuf;

use latte_lens::{
    app::{App, ContentMode, FocusPane, SearchMode, TabKind, TreeScope},
    config::TreeSide,
    ui,
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind},
};
use support::TestRepo;

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn modified_key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
    KeyEvent::new(code, modifiers)
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn mouse_down(column: u16, row: u16) -> MouseEvent {
    mouse(MouseEventKind::Down(MouseButton::Left), column, row)
}

fn settle(app: &mut App) {
    app.wait_for_background();
}

fn ready_app(path: PathBuf) -> anyhow::Result<App> {
    let mut app = App::with_system_open_disabled(path)?;
    // Pin the tree to the left and visible so layout assertions stay
    // deterministic; the right-side default and collapse are covered by
    // unit tests.
    app.set_tree_side(TreeSide::Left);
    app.set_tree_hidden(false);
    settle(&mut app);
    Ok(app)
}

fn render(app: &mut App) -> String {
    let backend = TestBackend::new(120, 30);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, app)).unwrap();
    format!("{:?}", terminal.backend().buffer())
}

fn fixture_repo() -> TestRepo {
    let repo = TestRepo::new();
    repo.write(
        "src/main.rs",
        "fn main() {\n    println!(\"hello\");\n}\n\nfn helper() -> i32 { 42 }\n",
    );
    repo.write(
        "src/lib.rs",
        "pub fn public_api() -> String {\n    \"api\".to_string()\n}\n",
    );
    repo.write(
        "README.md",
        "# Project\n\nSome markdown content.\n\n## Section\n\nMore text.\n",
    );
    repo.write("data.txt", "line one\nline two\nline three\n");
    repo.commit_all("fixture");
    repo
}

/// Select a file in the tree by navigating Down/Right.
fn select_file(app: &mut App) {
    app.handle_key(key(KeyCode::Down)); // src/
    app.handle_key(key(KeyCode::Right)); // expand
    app.handle_key(key(KeyCode::Down)); // main.rs
    settle(app);
}

// ---------------------------------------------------------------------------
// Tab lifecycle: open / close / switch / palette / menu / soft cap
// ---------------------------------------------------------------------------

#[test]
fn e2e_tab_lifecycle_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Starts with one Files tab.
    assert_eq!(app.tabs().len(), 1);
    assert_eq!(app.tab().kind(), TabKind::Files);

    // Ctrl+N opens the new-tab menu.
    app.handle_key(modified_key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert!(app.new_tab_menu.is_some());

    // Esc dismisses the menu.
    app.handle_key(key(KeyCode::Esc));
    assert!(app.new_tab_menu.is_none());

    // Open a Review tab through the menu.
    app.handle_key(modified_key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.tabs().len(), 2);
    assert_eq!(app.tab().kind(), TabKind::Review);
    assert_eq!(app.tree_scope, TreeScope::GitChanges);

    // Tab / Shift+Tab cycle.
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.tab().kind(), TabKind::Files);
    app.handle_key(key(KeyCode::BackTab));
    assert_eq!(app.tab().kind(), TabKind::Review);

    // Digit keys switch.
    app.handle_key(key(KeyCode::Char('1')));
    assert_eq!(app.tab().kind(), TabKind::Files);

    // Ctrl+W closes the active (first) tab, leaving the Review tab.
    app.handle_key(modified_key(KeyCode::Char('w'), KeyModifiers::CONTROL));
    assert_eq!(app.tabs().len(), 1);
    assert_eq!(app.tab().kind(), TabKind::Review);

    // Ctrl+W on the last tab is rejected.
    app.handle_key(modified_key(KeyCode::Char('w'), KeyModifiers::CONTROL));
    assert_eq!(app.tabs().len(), 1);

    // Digit beyond tab count is a no-op.
    app.handle_key(key(KeyCode::Char('9')));
    assert_eq!(app.tabs().len(), 1);

    // Open a Search tab.
    app.handle_key(modified_key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.tab().kind(), TabKind::Search);
    // Search tab opens the text search popup.
    assert!(app.search_mode().is_some());
    app.handle_key(key(KeyCode::Esc)); // close search popup

    // Open a second Files tab (same scope → projection populated).
    app.handle_key(modified_key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.tabs().len(), 3);
    assert_eq!(app.tab().kind(), TabKind::Files);
    assert!(!app.tab().files().visible_rows.is_empty());

    // Palette: open, type query, backspace, Esc.
    app.handle_key(modified_key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert!(app.tab_palette.is_some());
    app.handle_key(key(KeyCode::Char('r')));
    app.handle_key(key(KeyCode::Char('e')));
    app.handle_key(key(KeyCode::Char('v')));
    assert!(app.tab_palette.as_ref().unwrap().query.contains("rev"));
    app.handle_key(key(KeyCode::Backspace));
    app.handle_key(key(KeyCode::Backspace));
    assert_eq!(app.tab_palette.as_ref().unwrap().query, "r");
    app.handle_key(key(KeyCode::Up));
    app.handle_key(key(KeyCode::Esc));
    assert!(app.tab_palette.is_none());

    // Palette: type a filename, Enter opens the file.
    app.handle_key(modified_key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Char('m')));
    app.handle_key(key(KeyCode::Char('a')));
    app.handle_key(key(KeyCode::Char('i')));
    app.handle_key(key(KeyCode::Char('n')));
    // The palette should list main.rs as a file item (tabs don't match "main").
    let palette = app.tab_palette.as_ref().unwrap();
    assert!(
        !palette.items.is_empty(),
        "palette should have items after typing 'main'"
    );
    // Navigate to the file item (tabs don't match, so the first item is a file).
    app.handle_key(key(KeyCode::Enter));
    assert!(app.tab_palette.is_none());
    settle(&mut app);
    assert_eq!(app.tab().kind(), TabKind::Files);

    // Soft cap: open tabs until MAX_OPEN_TABS.
    while app.tabs().len() < 16 {
        app.handle_key(modified_key(KeyCode::Char('n'), KeyModifiers::CONTROL));
        app.handle_key(key(KeyCode::Enter));
    }
    assert_eq!(app.tabs().len(), 16);
    // The 17th is rejected.
    app.handle_key(modified_key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.tabs().len(), 16);
    assert!(app.last_error.is_some());
    assert!(
        app.last_error
            .as_deref()
            .unwrap()
            .contains("Tab limit reached")
    );
}

// ---------------------------------------------------------------------------
// Search: open, type, select, mouse, streaming
// ---------------------------------------------------------------------------

#[test]
fn e2e_search_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // '/' opens file search.
    app.handle_key(key(KeyCode::Char('/')));
    assert!(app.search_mode().is_some());
    assert_eq!(app.search_mode(), Some(SearchMode::Files));

    // Type a query.
    app.handle_key(key(KeyCode::Char('m')));
    app.handle_key(key(KeyCode::Char('a')));
    app.handle_key(key(KeyCode::Char('i')));
    app.handle_key(key(KeyCode::Char('n')));
    assert_eq!(app.search_query(), Some("main"));
    settle(&mut app);
    assert!(!app.search_results().is_empty());

    // Down selects the first result.
    app.handle_key(key(KeyCode::Down));
    // Enter opens the result.
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app);
    assert!(app.search_mode().is_none());

    // Ctrl+T opens text search.
    app.handle_key(modified_key(KeyCode::Char('t'), KeyModifiers::CONTROL));
    assert!(app.search_mode().is_some());
    assert_eq!(app.search_mode(), Some(SearchMode::Text));

    // Type a text query.
    app.handle_key(key(KeyCode::Char('h')));
    app.handle_key(key(KeyCode::Char('e')));
    app.handle_key(key(KeyCode::Char('l')));
    app.handle_key(key(KeyCode::Char('l')));
    app.handle_key(key(KeyCode::Char('o')));
    settle(&mut app);

    // Ctrl+U clears the query.
    app.handle_key(modified_key(KeyCode::Char('u'), KeyModifiers::CONTROL));
    assert_eq!(app.search_query(), Some(""));

    // Esc closes search.
    app.handle_key(key(KeyCode::Esc));
    assert!(app.search_mode().is_none());
}

// ---------------------------------------------------------------------------
// Preview find: Ctrl+F in preview mode
// ---------------------------------------------------------------------------

#[test]
fn e2e_preview_find_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Select a file and preview it.
    select_file(&mut app);
    assert_eq!(app.tab().content.mode, ContentMode::Preview);

    // Ctrl+F opens preview find.
    app.handle_key(modified_key(KeyCode::Char('f'), KeyModifiers::CONTROL));
    assert!(app.preview_find_is_active());

    // Type a find query.
    app.handle_key(key(KeyCode::Char('h')));
    app.handle_key(key(KeyCode::Char('e')));
    app.handle_key(key(KeyCode::Char('l')));
    app.handle_key(key(KeyCode::Char('l')));
    app.handle_key(key(KeyCode::Char('o')));
    settle(&mut app);
    assert_eq!(app.preview_find_query(), Some("hello"));

    // Enter finds the next match.
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app);

    // Esc closes preview find.
    app.handle_key(key(KeyCode::Esc));
    assert!(!app.preview_find_is_active());
}

// ---------------------------------------------------------------------------
// Content modes: preview / diff
// ---------------------------------------------------------------------------

#[test]
fn e2e_content_modes_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Select a file → Preview mode.
    select_file(&mut app);
    assert_eq!(app.tab().content.mode, ContentMode::Preview);

    // 'p' reloads preview.
    app.handle_key(key(KeyCode::Char('p')));
    settle(&mut app);
    assert_eq!(app.tab().content.mode, ContentMode::Preview);

    // Modify a file to make it changed.
    std::fs::write(repo.root().join("src/main.rs"), "fn changed() {}\n").unwrap();
    app.handle_key(key(KeyCode::Char('r'))); // refresh
    settle(&mut app);

    // Switch to Review scope.
    app.handle_key(modified_key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app);
    assert_eq!(app.tree_scope, TreeScope::GitChanges);

    // Select the changed file.
    app.handle_key(key(KeyCode::Down));
    settle(&mut app);

    // 'd' opens diff.
    app.handle_key(key(KeyCode::Char('d')));
    settle(&mut app);
    assert_eq!(app.tab().content.mode, ContentMode::Diff);
}

// ---------------------------------------------------------------------------
// Refresh
// ---------------------------------------------------------------------------

#[test]
fn e2e_refresh_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    let initial_count = app.all_entries.len();

    // Add a new file.
    repo.write("new-file.txt", "new content\n");
    repo.commit_all("add new file");

    // Refresh.
    app.handle_key(key(KeyCode::Char('r')));
    settle(&mut app);

    assert!(app.all_entries.len() > initial_count);
    assert!(
        app.all_entries
            .iter()
            .any(|e| e.relative == std::path::Path::new("new-file.txt"))
    );
}

// ---------------------------------------------------------------------------
// Folding
// ---------------------------------------------------------------------------

#[test]
fn e2e_folding_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Select main.rs.
    select_file(&mut app);
    assert_eq!(app.tab().content.mode, ContentMode::Preview);

    // [ folds the current block.
    app.handle_key(key(KeyCode::Char('[')));
    let rendered = render(&mut app);
    // The fold marker should appear (or the content changed).
    assert!(rendered.contains('▸') || rendered.contains("lines"));

    // ] unfolds.
    app.handle_key(key(KeyCode::Char(']')));
}

// ---------------------------------------------------------------------------
// Mouse: tab bar click, "+" button, menu item click
// ---------------------------------------------------------------------------

#[test]
fn e2e_mouse_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Render first to populate ui_regions.
    render(&mut app);

    // Open a Review tab.
    app.handle_key(modified_key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.tab().kind(), TabKind::Review);

    // Render to update ui_regions.
    render(&mut app);

    // Click the first tab in the tab bar (row 0).
    app.handle_mouse(mouse_down(2, 0));
    assert_eq!(app.tab().kind(), TabKind::Files);

    // Render to update ui_regions.
    render(&mut app);

    // Click the "+" button (right end of tab bar, col 118).
    app.handle_mouse(mouse_down(118, 0));
    assert!(app.new_tab_menu.is_some());

    // Render to update ui_regions.
    render(&mut app);

    // Click a menu item (Review, row 3 → index 1).
    app.handle_mouse(mouse_down(105, 3));
    assert!(app.new_tab_menu.is_none());
    assert_eq!(app.tab().kind(), TabKind::Review);
}

// ---------------------------------------------------------------------------
// Quit confirmation
// ---------------------------------------------------------------------------

#[test]
fn e2e_quit_confirmation_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // First q sets the confirmation.
    app.handle_key(key(KeyCode::Char('q')));
    assert!(!app.should_quit());

    // Second q within the window quits.
    app.handle_key(key(KeyCode::Char('q')));
    assert!(app.should_quit());
}

// ---------------------------------------------------------------------------
// Content snapshot
// ---------------------------------------------------------------------------

#[test]
fn e2e_content_snapshot_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Select a file and load content.
    select_file(&mut app);

    // The content should have lines.
    assert!(!app.tab().content.lines.is_empty());
}

// ---------------------------------------------------------------------------
// External open (disabled in tests)
// ---------------------------------------------------------------------------

#[test]
fn e2e_external_open_disabled_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Select a file.
    select_file(&mut app);

    // 'o' requests external open (disabled in test mode → error or no-op).
    app.handle_key(key(KeyCode::Char('o')));
    settle(&mut app);
}

// ---------------------------------------------------------------------------
// Focus pane switching
// ---------------------------------------------------------------------------

#[test]
fn e2e_focus_pane_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Default focus is Tree.
    assert_eq!(app.focused_pane, FocusPane::Tree);

    // 'l' switches to Content.
    app.handle_key(key(KeyCode::Char('l')));
    assert_eq!(app.focused_pane, FocusPane::Content);

    // 'h' switches back to Tree.
    app.handle_key(key(KeyCode::Char('h')));
    assert_eq!(app.focused_pane, FocusPane::Tree);
}

// ---------------------------------------------------------------------------
// Path copy
// ---------------------------------------------------------------------------

#[test]
fn e2e_path_copy_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Select a file.
    select_file(&mut app);

    // 'y' copies the relative path.
    app.handle_key(key(KeyCode::Char('y')));
    settle(&mut app);
    assert!(app.clipboard_status.is_some());
}

// ---------------------------------------------------------------------------
// Rendering: the app renders without panicking in various states
// ---------------------------------------------------------------------------

#[test]
fn e2e_render_states_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Render in the default state.
    let rendered = render(&mut app);
    assert!(rendered.contains("LATTE LENS"));

    // Open the new-tab menu and render.
    app.handle_key(modified_key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    let rendered = render(&mut app);
    assert!(rendered.contains("Review"));

    // Open the palette and render.
    app.handle_key(key(KeyCode::Esc));
    app.handle_key(modified_key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    let rendered = render(&mut app);
    assert!(rendered.contains('>'));

    // Open file search and render.
    app.handle_key(key(KeyCode::Esc));
    app.handle_key(key(KeyCode::Char('/')));
    let rendered = render(&mut app);
    assert!(rendered.contains("Search") || rendered.contains("search") || rendered.contains('>'));
}

// ---------------------------------------------------------------------------
// Multi-tab content independence
// ---------------------------------------------------------------------------

#[test]
fn e2e_multi_tab_content_independence_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Select main.rs in the first tab.
    select_file(&mut app);
    let first_content_len = app.tab().content.lines.len();
    assert!(first_content_len > 0);

    // Open a second Files tab.
    app.handle_key(modified_key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app);

    // Switch back to the first tab — content should be preserved.
    app.handle_key(key(KeyCode::Char('1')));
    settle(&mut app);
    assert_eq!(app.tab().content.lines.len(), first_content_len);
}

// ---------------------------------------------------------------------------
// Navigation history (Alt+Left / Alt+Right)
// ---------------------------------------------------------------------------

#[test]
fn e2e_navigation_history_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Select a file, then another, to build history.
    select_file(&mut app);

    app.handle_key(key(KeyCode::Down)); // lib.rs
    settle(&mut app);

    // Alt+Left goes back.
    app.handle_key(modified_key(KeyCode::Left, KeyModifiers::ALT));
    settle(&mut app);

    // Alt+Right goes forward.
    app.handle_key(modified_key(KeyCode::Right, KeyModifiers::ALT));
    settle(&mut app);
}

// ---------------------------------------------------------------------------
// Search mouse: click on search results
// ---------------------------------------------------------------------------

#[test]
fn e2e_search_mouse_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Open file search and type a query.
    app.handle_key(key(KeyCode::Char('/')));
    app.handle_key(key(KeyCode::Char('m')));
    app.handle_key(key(KeyCode::Char('a')));
    app.handle_key(key(KeyCode::Char('i')));
    app.handle_key(key(KeyCode::Char('n')));
    settle(&mut app);
    assert!(!app.search_results().is_empty());

    // Render to populate ui_regions.
    let rendered = render(&mut app);
    assert!(rendered.contains("main.rs"));

    // Navigate to the first result with Down and Enter.
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app);
    assert!(app.search_mode().is_none());

    // Open text search and type a query.
    app.handle_key(modified_key(KeyCode::Char('t'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Char('h')));
    app.handle_key(key(KeyCode::Char('e')));
    app.handle_key(key(KeyCode::Char('l')));
    app.handle_key(key(KeyCode::Char('l')));
    app.handle_key(key(KeyCode::Char('o')));
    settle(&mut app);

    // Down + Enter selects a text search result.
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app);
}

// ---------------------------------------------------------------------------
// Refresh with file changes
// ---------------------------------------------------------------------------

#[test]
fn e2e_refresh_with_changes_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    let initial_count = app.all_entries.len();

    // Add a new file and refresh.
    repo.write("new-file.txt", "new content\n");
    repo.commit_all("add new file");
    app.handle_key(key(KeyCode::Char('r')));
    settle(&mut app);
    assert!(app.all_entries.len() > initial_count);

    // Modify a file and refresh.
    std::fs::write(repo.root().join("src/main.rs"), "fn changed() {}\n").unwrap();
    app.handle_key(key(KeyCode::Char('r')));
    settle(&mut app);

    // Switch to Review scope to see the changed file.
    app.handle_key(modified_key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app);
    assert_eq!(app.tree_scope, TreeScope::GitChanges);
}

// ---------------------------------------------------------------------------
// Directory expansion
// ---------------------------------------------------------------------------

#[test]
fn e2e_directory_expansion_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Navigate to the src/ directory and expand it.
    // The tree shows directories and files; navigate Down to find src/.
    for _ in 0..5 {
        app.handle_key(key(KeyCode::Down));
    }
    settle(&mut app);

    // Try expanding with Right.
    app.handle_key(key(KeyCode::Right));
    settle(&mut app);

    // The app should not crash; render to verify.
    let rendered = render(&mut app);
    assert!(rendered.contains("LATTE LENS"));
}

// ---------------------------------------------------------------------------
// Preview find keys
// ---------------------------------------------------------------------------

#[test]
fn e2e_preview_find_keys_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    select_file(&mut app);

    // Open preview find.
    app.handle_key(modified_key(KeyCode::Char('f'), KeyModifiers::CONTROL));
    assert!(app.preview_find_is_active());

    // Type a query.
    app.handle_key(key(KeyCode::Char('h')));
    app.handle_key(key(KeyCode::Char('e')));
    app.handle_key(key(KeyCode::Char('l')));
    app.handle_key(key(KeyCode::Char('l')));
    app.handle_key(key(KeyCode::Char('o')));
    settle(&mut app);
    assert_eq!(app.preview_find_query(), Some("hello"));

    // Enter finds the next match.
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app);

    // Shift+Enter finds the previous match.
    app.handle_key(modified_key(KeyCode::Enter, KeyModifiers::SHIFT));
    settle(&mut app);

    // Esc closes preview find.
    app.handle_key(key(KeyCode::Esc));
    assert!(!app.preview_find_is_active());
}

// ---------------------------------------------------------------------------
// Folding markdown
// ---------------------------------------------------------------------------

#[test]
fn e2e_folding_markdown_headless() {
    let repo = TestRepo::new();
    repo.write(
        "doc.md",
        "# Title\n\nSome content.\n\n## Section\n\nMore content.\n\n## Another\n\nEnd.\n",
    );
    repo.commit_all("markdown fixture");
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Select the markdown file.
    app.handle_key(key(KeyCode::Down));
    settle(&mut app);
    assert_eq!(app.tab().content.mode, ContentMode::Preview);

    // [ folds the current section.
    app.handle_key(key(KeyCode::Char('[')));
    settle(&mut app);

    // ] unfolds.
    app.handle_key(key(KeyCode::Char(']')));
    settle(&mut app);

    // The app should not crash.
    assert!(!app.should_quit());
}

// ---------------------------------------------------------------------------
// Document symbols (Ctrl+S)
// ---------------------------------------------------------------------------

#[test]
fn e2e_document_symbols_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    select_file(&mut app);

    // Ctrl+S opens document symbols.
    app.handle_key(modified_key(KeyCode::Char('s'), KeyModifiers::CONTROL));
    settle(&mut app);

    // The symbols picker should be open (or the navigation status should show).
    // Close with Esc.
    app.handle_key(key(KeyCode::Esc));
}

// ---------------------------------------------------------------------------
// External open
// ---------------------------------------------------------------------------

#[test]
fn e2e_external_open_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    select_file(&mut app);

    // 'o' requests external open (disabled in test mode).
    app.handle_key(key(KeyCode::Char('o')));
    settle(&mut app);

    // The app should not crash.
    assert!(!app.should_quit());
}

// ---------------------------------------------------------------------------
// Git changes review toggle
// ---------------------------------------------------------------------------

#[test]
fn e2e_git_review_toggle_headless() {
    let repo = TestRepo::new();
    repo.write("changed.txt", "before\n");
    repo.commit_all("initial");
    repo.write("changed.txt", "after\n");
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Open a Review tab.
    app.handle_key(modified_key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app);
    assert_eq!(app.tree_scope, TreeScope::GitChanges);

    // Select the changed file.
    app.handle_key(key(KeyCode::Down));
    settle(&mut app);

    // Load the diff.
    app.handle_key(key(KeyCode::Char('d')));
    settle(&mut app);
    assert_eq!(app.tab().content.mode, ContentMode::Diff);

    // Space toggles review.
    app.handle_key(key(KeyCode::Char(' ')));
    settle(&mut app);
}

// ---------------------------------------------------------------------------
// Content mode cycling: Preview → Diff → Preview
// ---------------------------------------------------------------------------

#[test]
fn e2e_content_mode_cycle_headless() {
    let repo = TestRepo::new();
    repo.write("changed.txt", "before\n");
    repo.commit_all("initial");
    repo.write("changed.txt", "after\n");
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Select the changed file in Files scope.
    app.handle_key(key(KeyCode::Down));
    settle(&mut app);
    assert_eq!(app.tab().content.mode, ContentMode::Preview);

    // 'd' loads the diff.
    app.handle_key(key(KeyCode::Char('d')));
    settle(&mut app);
    assert_eq!(app.tab().content.mode, ContentMode::Diff);

    // 'p' loads the preview.
    app.handle_key(key(KeyCode::Char('p')));
    settle(&mut app);
    assert_eq!(app.tab().content.mode, ContentMode::Preview);
}

// ---------------------------------------------------------------------------
// Multi-tab content isolation: each tab keeps its own content
// ---------------------------------------------------------------------------

#[test]
fn e2e_multi_tab_content_isolation_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Select main.rs in the first tab.
    select_file(&mut app);
    let first_content = app.tab().content.lines.clone();
    assert!(!first_content.is_empty());

    // Open a second Files tab and select lib.rs.
    app.handle_key(modified_key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app);

    // Select lib.rs in the second tab.
    app.handle_key(key(KeyCode::Down)); // src/
    app.handle_key(key(KeyCode::Right)); // expand
    app.handle_key(key(KeyCode::Down)); // main.rs
    app.handle_key(key(KeyCode::Down)); // lib.rs
    settle(&mut app);

    // Switch back to the first tab — content should be main.rs.
    app.handle_key(key(KeyCode::Char('1')));
    settle(&mut app);
    assert_eq!(app.tab().content.lines, first_content);

    // Switch to the second tab — content should be lib.rs.
    app.handle_key(key(KeyCode::Char('2')));
    settle(&mut app);
    assert!(!app.tab().content.lines.is_empty());
    assert_ne!(app.tab().content.lines, first_content);
}

// ---------------------------------------------------------------------------
// Palette file open: type a filename and open it
// ---------------------------------------------------------------------------

#[test]
fn e2e_palette_file_open_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Open palette and type a filename.
    app.handle_key(modified_key(KeyCode::Char('p'), KeyModifiers::CONTROL));
    assert!(app.tab_palette.is_some());

    app.handle_key(key(KeyCode::Char('l')));
    app.handle_key(key(KeyCode::Char('i')));
    app.handle_key(key(KeyCode::Char('b')));
    settle(&mut app);

    // The palette should list lib.rs.
    let palette = app.tab_palette.as_ref().unwrap();
    assert!(!palette.items.is_empty());

    // Enter opens the file.
    app.handle_key(key(KeyCode::Enter));
    settle(&mut app);
    assert!(app.tab_palette.is_none());
    assert_eq!(app.tab().kind(), TabKind::Files);
}

// ---------------------------------------------------------------------------
// New tab menu: Up clamp and Esc
// ---------------------------------------------------------------------------

#[test]
fn e2e_new_tab_menu_navigation_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Open the menu.
    app.handle_key(modified_key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    assert!(app.new_tab_menu.is_some());

    // Up from the first item clamps at 0.
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.new_tab_menu.as_ref().unwrap().selected, 0);

    // Down to Review, then Up back to Files.
    app.handle_key(key(KeyCode::Down));
    assert_eq!(app.new_tab_menu.as_ref().unwrap().selected, 1);
    app.handle_key(key(KeyCode::Up));
    assert_eq!(app.new_tab_menu.as_ref().unwrap().selected, 0);

    // Esc dismisses.
    app.handle_key(key(KeyCode::Esc));
    assert!(app.new_tab_menu.is_none());
}

// ---------------------------------------------------------------------------
// Tab bar mouse click
// ---------------------------------------------------------------------------

#[test]
fn e2e_tab_bar_mouse_headless() {
    let repo = fixture_repo();
    let mut app = ready_app(repo.root().to_path_buf()).unwrap();

    // Open a Review tab.
    app.handle_key(modified_key(KeyCode::Char('n'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.tab().kind(), TabKind::Review);

    // Render to populate ui_regions.
    render(&mut app);

    // Click the first tab (Files) in the tab bar (row 0).
    app.handle_mouse(mouse_down(2, 0));
    assert_eq!(app.tab().kind(), TabKind::Files);

    // Render and click the "+" button.
    render(&mut app);
    app.handle_mouse(mouse_down(118, 0));
    assert!(app.new_tab_menu.is_some());

    // Render and click outside the menu to dismiss.
    render(&mut app);
    app.handle_mouse(mouse_down(50, 10));
    assert!(app.new_tab_menu.is_none());
}

// ---------------------------------------------------------------------------
// Quit confirmation
// ---------------------------------------------------------------------------
