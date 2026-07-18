"""Named production-binary journeys for completed Latte Lens features."""

from __future__ import annotations

import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Callable

from .fixtures import (
    ReadOnlyOracle,
    Sandbox,
    create_git_matrix_fixture,
    create_navigation_fixture,
    create_search_fixture,
)
from .terminal import E2EAssertionError, PtySession, TerminalScreen


FixtureBuilder = Callable[[Path, dict[str, str]], None]
Journey = Callable[["ScenarioContext"], None]


@dataclass(frozen=True)
class ScenarioCase:
    id: str
    group: str
    fixture: FixtureBuilder
    journey: Journey


@dataclass
class ScenarioContext:
    sandbox: Sandbox
    session: PtySession
    environment: dict[str, str]
    clipboard_mode: str
    oracle: ReadOnlyOracle

    @property
    def repository(self) -> Path:
        return self.sandbox.repository

    def write_text(self, relative: str, content: str) -> Path:
        path = self.repository / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        self.oracle.record_driver_write(path)
        return path

    def remove(self, relative: str) -> None:
        path = self.repository / relative
        path.unlink()
        self.oracle.record_driver_remove(path)


def wait_for_initial_files(session: PtySession) -> None:
    session.wait_raw((b"?1000h",), "initial mouse-enabled terminal")
    session.wait_screen(
        ("LATTE LENS", "1 Files", "2 Git changes", "Files", "Tree"),
        "initial all-files tree with tree focus",
        absent=(".git/",),
    )
    session.wait_until(
        lambda screen: (
            "Scanning files…" not in screen.text()
            and "Loading workspace…" not in screen.text()
            and (" loaded" in screen.text() or " entries" in screen.text())
        ),
        "initial filesystem and repository snapshot",
    )


def _click_disclosure(session: PtySession, marker: str) -> None:
    session.drain()
    divider = _interior_divider(session.screen) or session.screen.columns
    for row, cells in enumerate(session.screen.cells):
        line = "".join(cells[:divider])
        marker_column = line.find(marker)
        if marker_column < 0:
            continue
        disclosure = next(
            (
                column
                for column in range(marker_column - 1, -1, -1)
                if cells[column] in "▾▸"
            ),
            None,
        )
        if disclosure is not None:
            session.click(disclosure, row)
            return
    raise E2EAssertionError("input_target", f"cannot find tree disclosure for: {marker}")


def _click_tree_row(session: PtySession, marker: str) -> None:
    session.drain()
    divider = _interior_divider(session.screen) or session.screen.columns
    for row, cells in enumerate(session.screen.cells):
        line = "".join(cells[:divider])
        column = line.find(marker)
        if column >= 0:
            session.click(column, row)
            return
    raise E2EAssertionError("input_target", f"cannot find tree row for: {marker}")


def _click_marker_on_line(
    session: PtySession, marker: str, *, alongside: tuple[str, ...] = ()
) -> None:
    session.click(*_marker_position_on_line(session, marker, alongside=alongside))


def _marker_position_on_line(
    session: PtySession, marker: str, *, alongside: tuple[str, ...] = ()
) -> tuple[int, int]:
    session.drain()
    for row, cells in enumerate(session.screen.cells):
        line = "".join(cells)
        if marker in line and all(companion in line for companion in alongside):
            return line.index(marker), row
    raise E2EAssertionError(
        "input_target",
        f"cannot find {marker!r} alongside {alongside!r}",
    )


def _interior_divider(screen: TerminalScreen) -> int | None:
    candidates = [
        column
        for column in screen.vertical_rule_columns()
        if 4 < column < screen.columns - 5
    ]
    return candidates[0] if candidates else None


def _divider_near(screen: TerminalScreen, expected: int, tolerance: int = 2) -> bool:
    return any(abs(column - expected) <= tolerance for column in screen.vertical_rule_columns())


def files_navigation(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)
    session.wait_screen(
        ("a-dir",),
        "initial collapsed all-files directory",
        absent=("b-changed.rs",),
    )

    # Discover the current semantic divider instead of encoding a layout/style
    # coordinate. This keeps resize behavior covered while the TUI evolves.
    session.wait_until(lambda screen: _interior_divider(screen) is not None, "tree divider visible")
    original = _interior_divider(session.screen)
    assert original is not None
    row = min(10, session.screen.rows - 3)
    expanded = min(original + 9, session.screen.columns - 10)
    session.drag(original, row, expanded, row)
    session.wait_until(
        lambda screen: _divider_near(screen, expanded), "expanded tree divider"
    )
    current = _interior_divider(session.screen)
    assert current is not None
    session.drag(current, row, 0, row)
    session.wait_until(
        lambda screen: (_interior_divider(screen) or original) < original,
        "minimum tree divider constraint",
    )
    current = _interior_divider(session.screen)
    assert current is not None
    session.drag(current, row, session.screen.columns - 1, row)
    session.wait_until(
        lambda screen: (_interior_divider(screen) or 0) > original,
        "minimum content divider constraint",
    )
    current = _interior_divider(session.screen)
    assert current is not None
    session.drag(current, row, original, row)
    session.wait_until(
        lambda screen: _divider_near(screen, original), "restored tree divider"
    )

    session.key(b"h")
    session.key(b"\r")
    session.wait_screen(
        ("▾ a-dir", "nested"), "keyboard-opened collapsed All Files directory"
    )
    _click_disclosure(session, "a-dir")
    session.wait_screen(
        ("a-dir", "Tree"),
        "mouse-closed directory with one click",
        absent=("nested", "b-changed.rs"),
    )
    _click_disclosure(session, "a-dir")
    session.wait_screen(("a-dir", "nested"), "mouse-opened directory with one click")
    session.key(b"\r")
    session.wait_screen(
        ("a-dir",),
        "keyboard-closed directory",
        absent=("nested", "b-changed.rs"),
    )

    # Preserve the original cross-scope state assertion in the Files group.
    context.write_text("y-untracked.txt", "new file\n")
    session.key(b"2")
    session.wait_screen(
        ("Git changes", "b-changed.rs", "nested-owned.txt", "Diff", "diff --git"),
        "Git Changes available for Files scope-state check",
    )
    session.key(b"l")
    session.wait_screen(("Diff", "Content"), "Files journey content focus cue")
    session.key(b"h")
    session.key(b"\x1b[H")
    session.key(b"\x1b[A")
    session.wait_screen(("Git changes", "Tabs"), "Files journey scope-tabs focus cue")
    files_tab = session.screen.find("1 Files")
    if files_tab is None:
        raise E2EAssertionError("input_target", "Files scope tab is not visible")
    session.click(*files_tab)
    session.wait_screen(
        ("Files", "a-dir", "Tree"),
        "mouse-selected collapsed All Files scope",
        absent=("nested", "b-changed.rs"),
    )
    _click_tree_row(session, "z-clean.rs")
    session.wait_screen(
        ("Preview", "clean()", "Tree"), "mouse-selected All Files preview"
    )

    copy_position = session.screen.find("clean()")
    if copy_position is None:
        raise E2EAssertionError("input_target", "clean() preview text is not visible")
    copy_column, copy_row = copy_position
    copy_start = len(session.output)
    session.drag(copy_column, copy_row, copy_column + 4, copy_row)
    expected_status = (
        "Copied 5 characters"
        if context.clipboard_mode == "native"
        else "Sent 5 characters to terminal clipboard"
    )
    session.wait_screen((expected_status,), "mouse-release Preview clipboard copy")
    if context.clipboard_mode == "native":
        clipboard = subprocess.run(["pbpaste"], check=True, capture_output=True).stdout
        if clipboard != b"clean":
            raise E2EAssertionError(
                "clipboard", f"native clipboard mismatch: expected b'clean', got {clipboard!r}"
            )
    else:
        session.wait_raw(
            (b"\x1b]52;c;Y2xlYW4=\x07",),
            "exact Preview OSC 52 clipboard payload",
            start=copy_start,
        )
    session.key(b"\x03")
    session.wait_screen((expected_status,), "Ctrl+C Preview clipboard recopy")
    session.scroll_down(min(session.screen.columns - 5, copy_column + 20), copy_row)


def files_refresh(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)
    _click_tree_row(session, "z-clean.rs")
    session.wait_screen(("Preview", "clean()"), "refresh baseline selection preview")
    context.write_text("refresh-added.txt", "refresh content\n")
    session.key(b"r")
    session.wait_screen(
        ("refresh-added.txt", "clean()"),
        "Files refresh adds a file and preserves selected preview",
    )
    context.remove("refresh-added.txt")
    session.key(b"r")
    session.wait_screen(
        ("clean()",),
        "Files refresh removes a file and preserves selected preview",
        absent=("refresh-added.txt",),
    )


def keyboard_controls(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)

    # Move through the explicit Tabs -> Tree -> Content focus model and both
    # scope-tab arrow branches before exercising the global tab shortcuts.
    session.key(b"\x1b[A")
    session.wait_screen(("Tabs",), "Up from the first tree row focuses scope tabs")
    session.key(b"\x1b[C")
    session.wait_screen(("Git changes", "b-changed.rs", "Diff"), "Right tab selects Git Changes")
    session.key(b"\x1b[D")
    session.wait_screen(("Files", "a-dir", "Tabs"), "Left tab returns to Files")
    session.key(b"\x1b[B")
    session.wait_screen(("Tree",), "Down from tabs returns focus to the tree")
    session.key(b"\t")
    session.wait_screen(("Git changes", "Diff"), "Tab advances to Git Changes")
    session.key(b"\x1b[Z")
    session.wait_screen(("Files", "a-dir"), "BackTab returns to Files")

    # Tree navigation covers bounded first/last selection, activation, and
    # the transition into the content pane.
    session.key(b"G")
    session.wait_screen(("z-clean.rs", "Preview", "clean()"), "tree End selects the last file")
    session.key(b"\x1b[C")
    session.wait_screen(("Content",), "Right moves focus into content")
    for key in (
        b"j",
        b"k",
        b"\x1b[6~",
        b"\x1b[5~",
        b"\x04",
        b"\x15",
        b"\x1b[1;2C",
        b"\x1b[1;2D",
        b"G",
        b"g",
        b"\x1b[C",
    ):
        session.key(key)
    session.key(b"\x1b[D")
    session.key(b"g")
    session.key(b"\r")
    session.wait_screen(("▾ a-dir", "nested"), "tree Home and Enter expand the first directory")
    _click_disclosure(session, "nested")
    session.wait_screen(
        ("b-changed.rs",),
        "nested directory loading makes the changed file searchable",
    )

    # Wheel-up is separate from wheel-down in Crossterm and must route to the
    # pointed pane, independent of the current keyboard focus.
    tree_marker = session.screen.find("a-dir")
    if tree_marker is None:
        raise E2EAssertionError("input_target", "tree row is not visible for wheel routing")
    session.scroll_up(*tree_marker)
    content_column = min(session.screen.columns - 2, (_interior_divider(session.screen) or 40) + 8)
    session.scroll_up(content_column, min(10, session.screen.rows - 3))

    # Loading a diff from All Files must resolve the owning repository instead
    # of assuming the workspace root. Cycling here covers the All Files branch
    # of changed-file navigation before the Git Changes projection takes over.
    session.key(b"/")
    session.key(b"b-changed")
    session.wait_screen(("b-changed.rs", "Open File"), "file search finds the changed file")
    session.key(b"\r")
    session.wait_screen(("Preview", "changed()"), "file search opens the changed preview")
    session.key(b"d")
    session.wait_screen(("Diff", "diff --git"), "All Files resolves the owning repository diff")
    session.key(b"n")
    session.wait_screen(("Diff", "diff --git"), "All Files cycles to its next changed entry")
    session.key(b"N")
    session.wait_screen(("Diff", "diff --git"), "All Files cycles to its previous changed entry")

    # Changed-file navigation runs in both directions only while Diff owns the
    # content pane. Preview/Diff toggles then return to the same owning change.
    session.key(b"2")
    session.wait_screen(("Git changes", "Diff", "diff --git"), "Git Changes is ready for change cycling")
    session.wait_until(
        lambda screen: all(
            marker not in screen.text()
            for marker in ("Refreshing workspace", "Loading directory", "Loading content", "LOADING")
        ),
        "Git Changes refresh settles before directory interaction",
    )
    if session.screen.find("a-dir") is None:
        _click_disclosure(session, "root · main")
        session.wait_screen(("a-dir",), "Git root reopens after the scope refresh")
    _click_tree_row(session, "a-dir")
    session.wait_screen(
        ("changed file", "directory"),
        "Git directory selection renders its aggregate information",
    )
    session.key(b"\r")
    session.wait_screen(("b-changed.rs",), "Git directory reopens after its information view")
    _click_tree_row(session, "b-changed.rs")
    session.wait_screen(("Diff", "diff --git"), "Git changed-file selection restores the diff")
    session.key(b"n")
    session.wait_screen(("Diff", "diff --git"), "n selects the next changed file")
    session.key(b"N")
    session.wait_screen(("Diff", "diff --git"), "N selects the previous changed file")
    session.key(b"p")
    session.wait_screen(("Preview",), "p loads the changed file preview")
    session.key(b"d")
    session.wait_screen(("Diff", "diff --git"), "d restores the owning diff")
    session.click_marker("r  Refresh")
    session.wait_screen(("Git changes", "Diff"), "mouse refresh preserves Git Changes")


def git_navigation(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)
    context.write_text("y-untracked.txt", "new file\n")
    session.key(b"2")
    session.wait_screen(
        (
            "Git changes",
            "a-dir",
            "nested",
            "b-changed.rs",
            "vendor/nested",
            "nested-owned.txt",
            "Diff",
            "diff --git",
            "changed()",
        ),
        "refreshed Git Changes tree with expanded ancestors and diff",
    )

    session.key(b"\x1b[H")
    session.key(b"\r")
    session.wait_screen(
        ("." ,),
        "keyboard-collapsed repository group",
        absent=("b-changed.rs", "nested-owned.txt"),
    )
    session.key(b"\r")
    session.wait_screen(
        ("b-changed.rs", "nested-owned.txt"), "keyboard-reopened repository group"
    )
    _click_disclosure(session, ".")
    session.wait_screen(
        ("Git changes",),
        "mouse-collapsed repository group",
        absent=("b-changed.rs", "nested-owned.txt"),
    )
    _click_disclosure(session, ".")
    session.wait_screen(
        ("vendor/nested", "nested-owned.txt"), "mouse-reopened repository group"
    )

    session.key(b"\x1b[F")
    session.wait_screen(
        ("nested-owned.txt", "+nested changed"),
        "nested repository diff routing",
        absent=("+pub fn changed()",),
    )
    context.write_text("vendor/nested/second.txt", "second nested file\n")
    session.key(b"r")
    session.wait_screen(
        ("second.txt", "+nested changed"),
        "repository refresh with stable owning selection",
    )
    session.key(b"l")
    session.wait_screen(("Diff", "Content"), "visible content focus cue")
    session.key(b"h")
    session.key(b"\x1b[H")
    session.key(b"\x1b[A")
    session.wait_screen(("Git changes", "Tabs"), "visible scope-tabs focus cue")


def _line_with(screen: TerminalScreen, marker: str) -> str:
    divider = _interior_divider(screen) or screen.columns
    for cells in screen.cells:
        line = "".join(cells[:divider]).rstrip()
        if marker in line:
            return line
    return ""


def git_status_matrix(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)
    session.key(b"2")
    session.wait_screen(
        (
            "staged.txt",
            "worktree.txt",
            "both.txt",
            "deleted.txt",
            "renamed.txt",
            "untracked.txt",
            "Diff",
        ),
        "representative staged/worktree/both/deleted/rename/untracked status matrix",
    )
    expected_statuses = {
        "staged.txt": "ᴍ",
        "worktree.txt": "ᴍ",
        "both.txt": "ᴍᴍ",
        "deleted.txt": "D",
        "renamed.txt": "R",
        "untracked.txt": "??",
    }
    for marker, status in expected_statuses.items():
        line = _line_with(session.screen, marker)
        if status not in line:
            raise E2EAssertionError(
                "status_matrix", f"expected {status!r} on {marker!r} row, got {line!r}"
            )
        session.assertions.append(f"{marker} exposes {status} status")

    _click_tree_row(session, "worktree.txt")
    session.wait_screen(
        ("worktree.txt", "+worktree after"), "status matrix selects a worktree-only diff"
    )
    _click_tree_row(session, "both.txt")
    session.wait_screen(
        ("both.txt", "+both staged", "+both worktree"),
        "status matrix selects owning diff",
        absent=("LOADING",),
    )
    session.key(b"\x06")
    session.wait_screen(("Find", "0/0"), "in-diff find opens")
    session.key(b"both")
    session.wait_screen(("Find", "/"), "in-diff find accepts a query", absent=("0/0",))
    session.key(b"\x1b")
    session.wait_screen(("Diff", "both.txt"), "in-diff find closes back to diff", absent=(" Find ",))
    session.key(b"p")
    session.wait_screen(("Preview", "both worktree"), "changed file switches from Diff to Preview")
    session.key(b"d")
    session.wait_screen(("Diff", "diff --git"), "changed file switches back to Diff")


def git_review_state(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)
    session.key(b"2")
    session.wait_until(
        lambda screen: all(
            marker in _line_with(screen, "worktree.txt")
            for marker in ("○", "+1", "-1")
        ),
        "worktree diff row exposes unreviewed state and line counts",
    )

    _click_tree_row(session, "worktree.txt")
    session.wait_screen(
        ("worktree.txt", "+worktree after", "Space review"),
        "reviewable worktree diff is fully loaded",
    )
    session.key(b" ")
    session.wait_until(
        lambda screen: "✓" in _line_with(screen, "worktree.txt")
        and "1/6 reviewed" in screen.text(),
        "Space marks the current diff version reviewed",
    )

    context.write_text("worktree.txt", "worktree after changed\nsecond line\n")
    session.key(b"r")
    session.wait_until(
        lambda screen: all(
            marker in _line_with(screen, "worktree.txt")
            for marker in ("↻", "+2", "-1")
        )
        and "1 changed" in screen.text()
        and "+worktree after changed" in screen.text(),
        "refresh marks a reviewed file stale and loads its updated diff and line counts",
    )
    session.key(b" ")
    session.wait_until(
        lambda screen: "✓" in _line_with(screen, "worktree.txt")
        and "1/6 reviewed" in screen.text()
        and "1 changed" not in screen.text(),
        "Space reviews the refreshed diff version",
    )


def search_preview(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)
    session.key(b"/")
    session.wait_screen(("Open File", "File", "Text"), "file search opens")
    session.key(b"search-target")
    session.wait_screen(
        ("search-target.rs", "results"), "file search returns the matching path"
    )
    session.key(b"\r")
    session.wait_screen(
        ("Preview", "searchable()"), "file search opens selected preview", absent=("Open File",)
    )

    session.key(b"\x06")
    session.wait_screen(("Find", "0/0"), "Preview find opens")
    session.key(b"searchable")
    session.wait_screen(("Find", "1/1"), "Preview find locates current content")
    session.key(b"\r")
    session.wait_screen(("1/1",), "Preview find next wraps predictably")
    session.key(b"\x1b")
    session.wait_screen(("Preview", "searchable()"), "Preview find closes", absent=(" Find ",))

    session.key(b"\x14")
    session.wait_screen(("Search Workspace", "Aa", "Word", ".*", "Ign"), "text search opens")
    session.key(b"unique_workspace_phrase")
    session.wait_screen(
        ("search-target.rs:2", "unique_workspace_phrase"),
        "text search streams a workspace match",
    )
    session.key(b"\r")
    session.wait_screen(
        ("Preview", "unique_workspace_phrase"),
        "text search opens the selected result",
        absent=("Search Workspace",),
    )

    session.key(b"\x14")
    session.wait_screen(("Search Workspace",), "text search session reopens")
    session.key(b"\x15")
    session.key(b"ignored_unique_phrase")
    session.wait_screen(("No matches",), "ignored text is excluded by default")
    session.key(b"\x1b[15~")
    session.wait_screen(
        ("hidden.txt:1", "ignored_unique_phrase"), "F5 includes ignored search results"
    )
    session.key(b"\x10")
    session.wait_screen(("Open File", "File", "Text"), "Ctrl+P switches to file search")
    session.key(b"\x14")
    session.wait_screen(
        ("Search Workspace", "ignored_unique_phrase"),
        "Ctrl+T restores the text-search session",
    )
    session.key(b"\x1b")
    session.wait_screen(
        ("Preview", "unique_workspace_phrase"),
        "Esc restores the pre-search Preview",
        absent=("Search Workspace",),
    )


def search_controls(context: ScenarioContext) -> None:
    session = context.session
    wait_for_initial_files(session)

    # Ctrl+F on directory info must explain why find is unavailable instead of
    # opening a control that cannot search the current content mode.
    session.key(b"\x06")
    session.wait_screen(
        ("Open a file preview or diff before using Ctrl+F",),
        "Preview find explains its content precondition",
    )

    # Open the file picker through its real mouse hit box, exercise every query
    # editing/navigation branch, then open a production preview by double click.
    session.click_marker("/ Open")
    session.wait_screen(("Open File", "File", "Text"), "mouse opens file search")
    session.key(b"alpha beta")
    session.key(b"\x1b[H")
    session.key(b"\x1b[C")
    session.key(b"\x1b[3~")
    session.key(b"\x1b[F")
    session.key(b"\x1b[D")
    session.key(b"\x7f")
    session.key(b"\x17")
    session.key(b"\x15")
    session.wait_screen(("Type a file name or path",), "file query editing clears cleanly")
    session.key(b"search-target")
    session.wait_screen(
        ("search-target-other.rs", "search-target.rs", "results"),
        "edited file query converges",
    )
    for key in (b"\x1b[B", b"\x1b[A", b"\x1b[6~", b"\x1b[5~"):
        session.key(key)
    session.wait_screen(
        ("· src/search-target.rs",),
        "file-search navigation keeps the result selected",
    )
    # Give each click its own terminal input batch. Selecting the other result
    # first makes the first click on the target observable, so the second click
    # is only sent after the application has processed the first one.
    session.key(b"\x1b[B")
    session.wait_screen(
        ("▌ · src/search-target-other.rs",),
        "file-search selection moves away from the double-click target",
    )
    file_result = _marker_position_on_line(session, "· src/search-target.rs")
    session.click(*file_result)
    session.wait_screen(
        ("▌ · src/search-target.rs",),
        "first target click is processed before the second",
    )
    if context.environment.get("CARGO_LLVM_COV") == "1":
        # Coverage instrumentation can make the redraw between two separately
        # observed clicks exceed the product's 400 ms double-click window. The
        # regular Linux/macOS E2E jobs exercise that wall-clock interaction;
        # the coverage runner uses the equivalent acceptance path after proving
        # the production mouse hit box selected this result.
        session.key(b"\r")
    else:
        file_result = _marker_position_on_line(session, "· src/search-target.rs")
        session.click(*file_result)
    session.wait_screen(
        ("Preview", "searchable()"),
        "the selected file-search result is accepted",
        absent=("Open File",),
    )

    # Preview find keyboard and mouse controls share the production hit boxes.
    session.key(b"\x06")
    session.key(b"searchable")
    session.wait_screen(("Find", "1/1"), "Preview find has one result")
    session.key(b"\x1b[12~")
    session.key(b"\x1b[A")
    session.key(b"\x1b[B")
    session.key(b"\x1b[H")
    session.key(b"\x1b[C")
    session.key(b"\x1b[3~")
    session.key(b"\x1b[F")
    session.key(b"\x1b[D")
    session.key(b"\x7f")
    session.key(b"\x17")
    session.key(b"\x15")
    session.wait_screen(("Find", "0/0"), "Preview find editing reaches an empty query")
    session.key(b"searchable")
    session.wait_screen(("Find", "1/1"), "Preview find query is restored")
    _click_marker_on_line(session, "Aa", alongside=("Find", "1/1"))
    _click_marker_on_line(session, "↑", alongside=("Find", "1/1"))
    _click_marker_on_line(session, "↓", alongside=("Find", "1/1"))
    _click_marker_on_line(session, "Find", alongside=("1/1",))
    _click_marker_on_line(session, "[x]", alongside=("Find", "1/1"))
    session.wait_screen(
        ("Preview", "searchable()"),
        "Preview find closes through its mouse control",
        absent=(" Find ",),
    )

    # Handoff from Preview find to workspace search, then cover every text
    # option through both keyboard and mouse controls.
    session.key(b"\x06")
    session.key(b"\x14")
    session.wait_screen(("Search Workspace", "File", "Text"), "find hands off to text search")
    search_modes = _marker_position_on_line(session, "File Text")
    session.click(*search_modes)
    session.wait_screen(("Open File",), "mouse switches search to files")
    search_modes = _marker_position_on_line(session, "File Text")
    session.click(search_modes[0] + len("File "), search_modes[1])
    session.wait_screen(("Search Workspace",), "mouse switches search back to text")
    session.key(b"unique_workspace_phrase")
    session.wait_screen(
        ("search-target.rs:2", "unique_workspace_phrase"),
        "workspace query is ready for option controls",
    )
    for key in (b"\x1b[12~", b"\x1b[13~", b"\x1b[14~"):
        session.key(key)
    session.wait_screen(
        ("search-target.rs:2", "unique_workspace_phrase"),
        "case, word, and regex keyboard options preserve the exact match",
    )
    for marker in ("Aa", "Word", ".*", "Ign"):
        _click_marker_on_line(session, marker, alongside=("File", "Text", "Aa", "Word", ".*", "Ign"))
    session.wait_screen(
        ("search-target.rs:2", "unique_workspace_phrase"),
        "mouse search options preserve the result",
    )
    _click_marker_on_line(
        session,
        "unique_workspace_phrase",
        alongside=("Clear",),
    )
    result = session.screen.find("search-target.rs:2")
    if result is None:
        raise E2EAssertionError("input_target", "text-search result is not visible")
    session.scroll_down(*result)
    _click_marker_on_line(session, "Clear ×")
    session.wait_screen(("Type text to search the workspace",), "mouse clears text search")
    session.key(b"ignored_unique_phrase")
    session.wait_screen(
        ("hidden.txt:1", "ignored_unique_phrase"),
        "mouse-enabled ignored search finds the hidden fixture",
    )
    session.key(b"\r")
    session.wait_screen(
        ("Preview", "ignored_unique_phrase"),
        "Enter accepts a text-search result",
        absent=("Search Workspace",),
    )

    # Inactive header buttons must remain mouse-addressable after a search
    # session has restored its preview.
    session.click_marker("^T Text")
    session.wait_screen(("Search Workspace",), "mouse reopens text search from the header")
    _click_marker_on_line(session, "Esc ×")
    session.wait_screen(("Preview", "ignored_unique_phrase"), "mouse closes text search")


CASES = (
    ScenarioCase("files-navigation", "files", create_navigation_fixture, files_navigation),
    ScenarioCase("files-refresh", "files", create_navigation_fixture, files_refresh),
    ScenarioCase("keyboard-controls", "files", create_navigation_fixture, keyboard_controls),
    ScenarioCase("git-navigation", "git-changes", create_navigation_fixture, git_navigation),
    ScenarioCase("git-status-matrix", "git-changes", create_git_matrix_fixture, git_status_matrix),
    ScenarioCase("git-review-state", "git-changes", create_git_matrix_fixture, git_review_state),
    ScenarioCase("search-preview", "search-preview", create_search_fixture, search_preview),
    ScenarioCase("search-controls", "search-preview", create_search_fixture, search_controls),
)


def selected_cases(group: str) -> list[ScenarioCase]:
    if group == "all":
        return list(CASES)
    return [case for case in CASES if case.group == group]


def validate_clipboard_mode(mode: str) -> None:
    if mode not in ("native", "osc52"):
        raise ValueError("LATTELENS_E2E_CLIPBOARD must be native or osc52")
    if mode == "native" and sys.platform != "darwin":
        raise ValueError("native clipboard E2E is currently supported only on macOS")
