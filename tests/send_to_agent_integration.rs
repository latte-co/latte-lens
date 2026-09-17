//! Integration tests for send-selection-to-agent (Herdr-style backend).

mod support;

use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
};

use latte_lens::{
    app::{App, ContentMode, FocusPane},
    config::TreeSide,
    send_agent::{AgentDiscovery, AgentLifecycle, AgentTarget, AgentTargetProvider, SendPhase},
    ui,
};
use ratatui::{
    Terminal,
    backend::TestBackend,
    crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind},
};

#[derive(Default)]
struct FakeAgentState {
    sends: Vec<(String, String)>,
    focus: Vec<String>,
}

struct FakeAgentProvider {
    inner: Arc<Mutex<FakeAgentState>>,
    available: bool,
    targets: Vec<AgentTarget>,
}

impl FakeAgentProvider {
    fn new(available: bool, targets: Vec<AgentTarget>) -> (Arc<Self>, Arc<Mutex<FakeAgentState>>) {
        let inner = Arc::new(Mutex::new(FakeAgentState::default()));
        let provider = Arc::new(Self {
            inner: Arc::clone(&inner),
            available,
            targets,
        });
        (provider, inner)
    }
}

impl AgentTargetProvider for FakeAgentProvider {
    fn available(&self) -> bool {
        self.available
    }

    fn discover(&self, _workspace_root: &Path) -> anyhow::Result<AgentDiscovery> {
        Ok(AgentDiscovery {
            targets: self.targets.clone(),
        })
    }

    fn send_draft(&self, pane_id: &str, text: &str) -> anyhow::Result<()> {
        self.inner
            .lock()
            .unwrap()
            .sends
            .push((pane_id.to_owned(), text.to_owned()));
        Ok(())
    }

    fn focus_pane(&self, pane_id: &str) -> anyhow::Result<()> {
        self.inner.lock().unwrap().focus.push(pane_id.to_owned());
        Ok(())
    }
}

fn target(agent: &str, pane: &str, status: AgentLifecycle, cwd: &Path) -> AgentTarget {
    let selectable = !matches!(status, AgentLifecycle::Blocked);
    AgentTarget {
        agent: agent.to_owned(),
        pane_id: pane.to_owned(),
        title: format!("{pane} work"),
        cwd: cwd.to_path_buf(),
        status,
        selectable,
        same_workspace: true,
    }
}

fn ready_app_with(
    root: &Path,
    provider: Arc<dyn AgentTargetProvider>,
) -> (App, ratatui::Terminal<TestBackend>) {
    let mut app = App::with_agent_provider(root.to_path_buf(), provider).unwrap();
    app.set_tree_side(TreeSide::Left);
    app.set_tree_hidden(false);
    app.focused_pane = FocusPane::Content;
    app.wait_for_background();
    assert_eq!(app.tab().content.mode, ContentMode::Preview);
    let backend = TestBackend::new(100, 24);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    (app, terminal)
}

/// Pump the event loop until the agent picker reaches `phase` (or closed),
/// driving the real background worker through its completion channel.
fn wait_phase(app: &mut App, predicate: impl Fn(SendPhase) -> bool) {
    for _ in 0..200 {
        app.poll_background();
        if predicate(app.send_to_agent.phase) {
            return;
        }
        if !app.wait_background_once() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    panic!(
        "picker never reached expected phase: {:?}",
        app.send_to_agent.phase
    );
}

fn mouse(kind: MouseEventKind, column: u16, row: u16, modifiers: KeyModifiers) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers,
    }
}

/// Drag-select a few characters of the first preview row.
fn select_first_row(app: &mut App, drag_modifiers: KeyModifiers) {
    let content_x = app.ui_regions.content_inner.x;
    let row = app.ui_regions.content_inner.y;
    let text_x = content_x + 4; // past the gutter "1 │ "
    app.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        text_x + 1,
        row,
        KeyModifiers::NONE,
    ));
    app.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        text_x + 5,
        row,
        drag_modifiers,
    ));
}

#[test]
fn ctrl_e_opens_picker_and_enter_delivers_draft_without_submitting() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("single.txt"), "alpha beta\nsecond\n").unwrap();
    let (provider, calls) = FakeAgentProvider::new(
        true,
        vec![
            target("claude", "w1:p2", AgentLifecycle::Idle, directory.path()),
            target("codex", "w1:p3", AgentLifecycle::Working, directory.path()),
        ],
    );
    let (mut app, mut terminal) = ready_app_with(directory.path(), provider);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    select_first_row(&mut app, KeyModifiers::NONE);
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        app.ui_regions.content_inner.x + 9,
        app.ui_regions.content_inner.y,
        KeyModifiers::NONE,
    ));
    assert_eq!(app.selected_content_text().as_deref(), Some("lpha "));

    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    wait_phase(&mut app, |phase| phase == SendPhase::Picking);
    assert_eq!(app.send_to_agent.targets.len(), 2);

    // Enter delivers to the first selectable target. The default template
    // wraps the selection with a source anchor; .txt opens a bare fence.
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    wait_phase(&mut app, |phase| phase == SendPhase::Closed);

    let state = calls.lock().unwrap();
    assert_eq!(
        state.sends,
        vec![(
            "w1:p2".to_owned(),
            "single.txt:1\n```\nlpha \n```".to_owned()
        )]
    );
    assert_eq!(state.focus, vec!["w1:p2".to_owned()]);
    drop(state);
    // The selection is consumed after a successful delivery.
    assert!(app.selected_content_text().is_none());
}

#[test]
fn typed_annotation_is_wrapped_above_the_fenced_selection() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("code.rs"), "fn main() {}\n").unwrap();
    let (provider, calls) = FakeAgentProvider::new(
        true,
        vec![target(
            "claude",
            "w1:p2",
            AgentLifecycle::Idle,
            directory.path(),
        )],
    );
    let (mut app, mut terminal) = ready_app_with(directory.path(), provider);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    select_first_row(&mut app, KeyModifiers::NONE);
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        app.ui_regions.content_inner.x + 9,
        app.ui_regions.content_inner.y,
        KeyModifiers::NONE,
    ));
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    wait_phase(&mut app, |phase| phase == SendPhase::Picking);

    // Typing lands directly in the annotation field (no mode shortcut).
    for ch in "why no buffer?".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
    }
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    wait_phase(&mut app, |phase| phase == SendPhase::Closed);

    let payload = &calls.lock().unwrap().sends[0].1;
    assert_eq!(payload, "code.rs:1\n▎why no buffer?\n\n```rust\nn mai\n```");
}

#[test]
fn tab_cycles_to_plain_and_sends_raw_selection() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("single.txt"), "alpha beta\n").unwrap();
    let (provider, calls) = FakeAgentProvider::new(
        true,
        vec![target(
            "claude",
            "w1:p2",
            AgentLifecycle::Idle,
            directory.path(),
        )],
    );
    let (mut app, mut terminal) = ready_app_with(directory.path(), provider);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    select_first_row(&mut app, KeyModifiers::NONE);
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        app.ui_regions.content_inner.x + 9,
        app.ui_regions.content_inner.y,
        KeyModifiers::NONE,
    ));
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    wait_phase(&mut app, |phase| phase == SendPhase::Picking);

    // One Tab switches the anchor template to plain text.
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    wait_phase(&mut app, |phase| phase == SendPhase::Closed);

    assert_eq!(
        calls.lock().unwrap().sends,
        vec![("w1:p2".to_owned(), "lpha ".to_owned())]
    );
}

#[test]
fn picker_renders_annotation_input_and_anchor_preview() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("code.rs"), "fn main() {}\n").unwrap();
    let (provider, _calls) = FakeAgentProvider::new(
        true,
        vec![target(
            "claude",
            "w1:p2",
            AgentLifecycle::Idle,
            directory.path(),
        )],
    );
    let (mut app, mut terminal) = ready_app_with(directory.path(), provider);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    select_first_row(&mut app, KeyModifiers::NONE);
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        app.ui_regions.content_inner.x + 9,
        app.ui_regions.content_inner.y,
        KeyModifiers::NONE,
    ));
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    wait_phase(&mut app, |phase| phase == SendPhase::Picking);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let buffer = terminal.backend().buffer();
    let screen: String = buffer
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>()
        .join("");
    assert!(screen.contains("code.rs:1"), "anchor status visible");
    assert!(screen.contains("n mai"), "payload preview visible");
    assert!(
        screen.contains("Type a question or instruction"),
        "annotation placeholder visible"
    );

    // Typing replaces the placeholder with the draft note.
    app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    let screen: String = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<Vec<_>>()
        .join("");
    assert!(screen.contains("▎q"));
    assert!(!screen.contains("Type a question or instruction"));
}

#[test]
fn entry_stays_inert_when_backend_unavailable() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("single.txt"), "alpha beta\n").unwrap();
    let (provider, _calls) = FakeAgentProvider::new(false, Vec::new());
    let (mut app, mut terminal) = ready_app_with(directory.path(), provider);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    select_first_row(&mut app, KeyModifiers::NONE);
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        app.ui_regions.content_inner.x + 9,
        app.ui_regions.content_inner.y,
        KeyModifiers::NONE,
    ));
    assert!(app.selected_content_text().is_some());

    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    assert!(!app.send_to_agent.is_open());
    assert!(!app.send_agent_footer_active());
}

#[test]
fn blocked_only_sessions_close_picker_without_selectable_target() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("single.txt"), "alpha beta\n").unwrap();
    let (provider, _calls) = FakeAgentProvider::new(
        true,
        vec![target(
            "claude",
            "w1:p9",
            AgentLifecycle::Blocked,
            directory.path(),
        )],
    );
    let (mut app, mut terminal) = ready_app_with(directory.path(), provider);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    select_first_row(&mut app, KeyModifiers::NONE);
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        app.ui_regions.content_inner.x + 9,
        app.ui_regions.content_inner.y,
        KeyModifiers::NONE,
    ));
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    wait_phase(&mut app, |phase| phase == SendPhase::Closed);
    assert!(!app.send_to_agent.is_open());
}

#[test]
fn ctrl_armed_drag_release_opens_picker_and_plain_release_does_not() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("single.txt"), "alpha beta\n").unwrap();
    let (provider, _calls) = FakeAgentProvider::new(
        true,
        vec![target(
            "claude",
            "w1:p2",
            AgentLifecycle::Idle,
            directory.path(),
        )],
    );
    let provider: Arc<dyn AgentTargetProvider> = provider;

    // Plain drag release: no picker.
    let (mut app, mut terminal) = ready_app_with(directory.path(), provider);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    select_first_row(&mut app, KeyModifiers::NONE);
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        app.ui_regions.content_inner.x + 9,
        app.ui_regions.content_inner.y,
        KeyModifiers::NONE,
    ));
    assert!(!app.send_to_agent.is_open());
    assert!(app.selected_content_text().is_some());

    // Armed drag release: tap Ctrl on the drag motion, release without Ctrl.
    let up_x = app.ui_regions.content_inner.x + 9;
    let up_y = app.ui_regions.content_inner.y;
    // Selection already exists from the plain gesture; start a fresh drag.
    app.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        app.ui_regions.content_inner.x + 5,
        up_y,
        KeyModifiers::NONE,
    ));
    app.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        up_x,
        up_y,
        KeyModifiers::CONTROL,
    ));
    assert!(app.content_selection_send_armed());
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        up_x,
        up_y,
        KeyModifiers::NONE,
    ));
    assert!(app.send_to_agent.is_open());
    wait_phase(&mut app, |phase| phase == SendPhase::Picking);
}

#[test]
fn esc_closes_picker_and_keeps_selection() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("single.txt"), "alpha beta\n").unwrap();
    let (provider, calls) = FakeAgentProvider::new(
        true,
        vec![target(
            "claude",
            "w1:p2",
            AgentLifecycle::Idle,
            directory.path(),
        )],
    );
    let (mut app, mut terminal) = ready_app_with(directory.path(), provider);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    select_first_row(&mut app, KeyModifiers::NONE);
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        app.ui_regions.content_inner.x + 9,
        app.ui_regions.content_inner.y,
        KeyModifiers::NONE,
    ));
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    wait_phase(&mut app, |phase| phase == SendPhase::Picking);

    app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.send_to_agent.is_open());
    assert!(app.selected_content_text().is_some());
    assert!(calls.lock().unwrap().sends.is_empty());
}

/// Load the working-tree diff for the current file and wait until a hunk is
/// displayed.
fn open_diff(app: &mut App) {
    app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
    for _ in 0..200 {
        app.poll_background();
        app.wait_background_once();
        if app.tab().content.mode == ContentMode::Diff
            && app
                .tab()
                .content
                .lines
                .iter()
                .any(|line| line.starts_with("@@"))
        {
            return;
        }
    }
    assert_eq!(app.tab().content.mode, ContentMode::Diff);
}

/// Drag-select a rectangle of diff content rows by line index. Small diffs
/// render at scroll offset zero.
fn select_diff_rows(app: &mut App, start_line: usize, end_line: usize, width: u16) {
    let content_x = app.ui_regions.content_inner.x;
    let row0 = app.ui_regions.content_inner.y;
    let text_x = content_x + app.content_gutter_width() as u16;
    app.handle_mouse(mouse(
        MouseEventKind::Down(MouseButton::Left),
        text_x,
        row0 + start_line as u16,
        KeyModifiers::NONE,
    ));
    app.handle_mouse(mouse(
        MouseEventKind::Drag(MouseButton::Left),
        text_x + width,
        row0 + end_line as u16,
        KeyModifiers::NONE,
    ));
    app.handle_mouse(mouse(
        MouseEventKind::Up(MouseButton::Left),
        text_x + width,
        row0 + end_line as u16,
        KeyModifiers::NONE,
    ));
}

#[test]
fn diff_hunk_gets_file_anchor_with_a_diff_fence() {
    use support::TestRepo;

    let repo = TestRepo::new();
    repo.write("single.txt", "alpha beta\nsecond line\n");
    repo.commit_all("initial");
    repo.write("single.txt", "alpha BETA changed\nsecond line\n");

    let (provider, calls) = FakeAgentProvider::new(
        true,
        vec![target("claude", "w1:p2", AgentLifecycle::Idle, repo.root())],
    );
    let (mut app, mut terminal) = ready_app_with(repo.root(), provider);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    open_diff(&mut app);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    // Hunk layout: line 6 is "-alpha beta", line 7 "+alpha BETA changed".
    select_diff_rows(&mut app, 6, 7, 10);
    let selected = app
        .selected_content_text()
        .expect("a non-empty diff hunk selection");
    assert!(selected.lines().any(|l| l.starts_with('-')));
    assert!(selected.lines().any(|l| l.starts_with('+')));

    assert!(app.send_agent_footer_active());
    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    wait_phase(&mut app, |phase| phase == SendPhase::Picking);

    // The nearest diff --git header supplies the file; the addition's new
    // line number is 1. The patch is wrapped in a ```diff fence verbatim.
    let anchor = app
        .send_to_agent
        .anchor
        .as_ref()
        .expect("single-file diff anchor");
    assert_eq!(anchor.path, "single.txt");
    assert_eq!((anchor.start_line, anchor.end_line), (1, 1));
    assert_eq!(anchor.language, Some("diff"));
    assert!(!anchor.gutter);

    let payload = app.send_to_agent.built_payload().text;
    assert!(payload.starts_with("single.txt:1\n```diff\n"));
    assert!(payload.trim_end().ends_with("```"));
    assert!(payload.contains("-alpha beta"));
    assert!(payload.contains("+alpha BETA"));

    // Tab still drops back to the raw patch text.
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert_eq!(
        app.send_to_agent.template,
        latte_lens::send_agent::SendTemplate::Plain
    );
    assert_eq!(
        app.send_to_agent.built_payload().text,
        selected.trim_end_matches('\n')
    );

    app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    wait_phase(&mut app, |phase| phase == SendPhase::Closed);
    assert_eq!(calls.lock().unwrap().sends.len(), 1);
}

#[test]
fn pure_deletion_hunk_anchors_at_the_hunk_new_start() {
    use support::TestRepo;

    let repo = TestRepo::new();
    repo.write("single.txt", "keep\ngone\n");
    repo.commit_all("initial");
    repo.write("single.txt", "keep\n");

    let (provider, _calls) = FakeAgentProvider::new(
        true,
        vec![target("claude", "w1:p2", AgentLifecycle::Idle, repo.root())],
    );
    let (mut app, mut terminal) = ready_app_with(repo.root(), provider);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();
    open_diff(&mut app);
    terminal.draw(|frame| ui::draw(frame, &mut app)).unwrap();

    // @@ -1,2 +1 @@ / " keep" / "-gone": the deletion row has no new line.
    let gone_row = app
        .tab()
        .content
        .lines
        .iter()
        .position(|line| line == "-gone")
        .unwrap();
    select_diff_rows(&mut app, gone_row, gone_row, 6);
    assert!(app.selected_content_text().is_some());

    app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
    wait_phase(&mut app, |phase| phase == SendPhase::Picking);
    let anchor = app
        .send_to_agent
        .anchor
        .as_ref()
        .expect("deletion still anchors");
    assert_eq!(anchor.path, "single.txt");
    assert_eq!(anchor.start_line, 1, "falls back to the hunk's +1 start");
    let payload = app.send_to_agent.built_payload().text;
    assert_eq!(payload, "single.txt:1\n```diff\n-gone\n```");
}
